// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! AV1 "Low Overhead Bitstream Format" (OBU) parsing, scoped to exactly
//! what `av1C` (the AV1 Codec Configuration Box's
//! `AV1CodecConfigurationRecord`) and `CMVideoFormatDescriptionCreate`
//! need: locating a Sequence Header OBU within a Temporal Unit and reading
//! out of it `seq_profile`/`seq_level_idx[0]`/`seq_tier[0]`/the
//! colour-config bits/the coded dimensions. No unsafe, no FFI -- purely a
//! byte-oriented parser, testable without any Apple API.
//!
//! AV1 does not use H.264/HEVC-style VPS/SPS/PPS parameter sets, and the
//! vendored `apple-cf`/`videotoolbox` bindings this codebase uses expose no
//! `CMVideoFormatDescriptionCreateFromAV1ParameterSets`-equivalent (grep
//! confirms no such symbol in either crate). `VideoToolbox` instead expects
//! an `av1C` box supplied through the format description's
//! `SampleDescriptionExtensionAtoms` extension -- see
//! `platform::make_av1_format_description`, which this module feeds.
//!
//! Every field, bit width and branch below is transcribed directly from
//! the normative syntax tables in AOMediaCodec/av1-spec
//! `06.bitstream.syntax.md` (`sequence_header_obu()`/`color_config()`/
//! `obu_header()`/`open_bitstream_unit()`) and AOMediaCodec/av1-spec
//! `04.conventions.md` (the `f(n)`/`leb128()` descriptors and their exact
//! bit-parsing algorithms), cross-checked against
//! AOMediaCodec/av1-spec `03.symbols.md` for the two named constants used
//! in a control-flow decision (`SELECT_SCREEN_CONTENT_TOOLS` /
//! `SELECT_INTEGER_MV`, both `2`, hence both always non-zero) and
//! AOMediaCodec/av1-spec `07.bitstream.semantics.md` for the `obu_type`
//! numeric table. The `av1C` byte layout is transcribed from
//! AOMediaCodec/av1-isobmff `index.bs` section 2.3.3
//! (`AV1CodecConfigurationRecord`) and independently cross-checked against
//! three shipping decoders that build this same `VideoToolbox`
//! `av1C`/`SampleDescriptionExtensionAtoms` extension: Chromium's
//! `media/gpu/mac/vt_config_util.mm`, Firefox's
//! `dom/media/platforms/apple/AppleVTDecoder.cpp`, and the from-scratch
//! Swift implementation in `alvr-org/alvr-visionos`'s
//! `ALVRClient/AV1Parser.swift` (which independently arrives at the exact
//! same bit-packing this module uses).
//!
//! One documented gap: `timing_info_present_flag == 1` is rejected rather
//! than parsed (`timing_info()`/`decoder_model_info()`/
//! `operating_parameters_info()` are not implemented). This is irrelevant
//! to a live, low-latency remote-desktop stream with no container-level
//! timing model -- AV1-in-ISOBMFF itself recommends
//! `timing_info_present_flag` be `0` for exactly this reason ("The
//! presentation times of AV1 samples are given by the ISOBMFF
//! structures").

/// Big-endian, MSB-first bit reader for AV1's `f(n)` syntax elements
/// (av1-spec `04.conventions.md`: "Unsigned n-bit number appearing
/// directly in the bitstream. The bits are read from high to low
/// order.").
struct BitReader<'a> {
    bytes: &'a [u8],
    byte_pos: usize,
    /// Next bit to read within `bytes[byte_pos]`, MSB-first, 0..=7.
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn read_bit(&mut self) -> Result<bool, String> {
        let byte = *self
            .bytes
            .get(self.byte_pos)
            .ok_or_else(|| "AV1 sequence header OBU is truncated".to_string())?;
        let bit = (byte >> (7 - self.bit_pos)) & 1 != 0;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Ok(bit)
    }

    /// `f(n)`: the next `n_bits`, most-significant-bit-first.
    fn f(&mut self, n_bits: u32) -> Result<u32, String> {
        debug_assert!(n_bits <= 32);
        let mut value = 0u32;
        for _ in 0..n_bits {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Ok(value)
    }

    /// `f(n)` for `n <= 8`, as a byte.
    fn f_u8(&mut self, n_bits: u32) -> Result<u8, String> {
        u8::try_from(self.f(n_bits.min(8))?).map_err(|_| "AV1 field wider than a byte".to_string())
    }
}

/// `leb128()` (av1-spec `04.conventions.md`): up to 8 little-endian
/// base-128 bytes, continuation flagged by each byte's most
/// significant bit. Returns `(value, bytes_consumed)`.
fn read_leb128(bytes: &[u8]) -> Result<(u64, usize), String> {
    let mut value: u64 = 0;
    for (i, byte) in bytes.iter().copied().enumerate().take(8) {
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    Err("AV1 leb128 does not terminate within 8 bytes".to_string())
}

/// `obu_type` (av1-spec `07.bitstream.semantics.md`'s numeric table).
/// Only [`ObuType::SequenceHeader`] is acted on here; every other kind
/// is passed through unmodified as part of the sample payload (see
/// `platform::decode_av1`) -- AV1 tolerates, and this codebase relies
/// on, `VideoToolbox` seeing whatever real mix of OBUs the encoder
/// produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObuType {
    SequenceHeader,
    TemporalDelimiter,
    FrameHeader,
    TileGroup,
    Metadata,
    Frame,
    RedundantFrameHeader,
    TileList,
    Padding,
    Reserved(u8),
}

const fn obu_type_from_bits(bits: u8) -> ObuType {
    match bits {
        1 => ObuType::SequenceHeader,
        2 => ObuType::TemporalDelimiter,
        3 => ObuType::FrameHeader,
        4 => ObuType::TileGroup,
        5 => ObuType::Metadata,
        6 => ObuType::Frame,
        7 => ObuType::RedundantFrameHeader,
        8 => ObuType::TileList,
        15 => ObuType::Padding,
        other => ObuType::Reserved(other),
    }
}

/// One parsed `open_bitstream_unit()` (av1-spec
/// `06.bitstream.syntax.md`): `obu_type` plus the byte ranges of the
/// whole OBU (header + `obu_size` leb128, if present, + payload) and
/// just its payload, both borrowed from the input so a Sequence Header
/// OBU can be re-embedded byte-for-byte into `av1C`'s `configOBUs` --
/// copying the real bytes makes "the values in configOBUs match the
/// `AV1CodecConfigurationRecord`" (av1-isobmff `index.bs`) a byte
/// identity, not a hope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    pub obu_type: ObuType,
    pub whole: &'a [u8],
    pub payload: &'a [u8],
}

/// Walks a "Low Overhead Bitstream Format" byte buffer (av1-spec's own
/// "Low overhead bitstream format" section; av1-isobmff `index.bs`'s
/// "AV1 Sample Format": one Temporal Unit's worth of concatenated
/// OBUs) into individual OBUs.
///
/// Every OBU must set `obu_has_size_field`, except optionally the last
/// one in the buffer, whose size then defaults to "the rest of the
/// buffer" -- both are explicitly sanctioned by av1-isobmff's "AV1
/// Sample Format" section. `obu_extension_flag` (temporal/spatial layer
/// id) is skipped, not interpreted: Arcen has no scalable/multi-layer
/// AV1 use case.
///
/// # Errors
///
/// Returns `Err` for `obu_forbidden_bit` set, a header or `obu_size`
/// that runs past the end of `data`, or a claimed payload length that
/// would too.
pub fn parse_obus(data: &[u8]) -> Result<Vec<Obu<'_>>, String> {
    let mut obus = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let header_byte = data[offset];
        if header_byte & 0x80 != 0 {
            return Err(format!(
                "AV1 OBU at offset {offset} has obu_forbidden_bit set"
            ));
        }
        let obu_type = obu_type_from_bits((header_byte >> 3) & 0x0f);
        let extension_flag = (header_byte >> 2) & 1 != 0;
        let has_size_field = (header_byte >> 1) & 1 != 0;
        let header_len = if extension_flag { 2 } else { 1 };
        if offset + header_len > data.len() {
            return Err(format!(
                "AV1 OBU at offset {offset} is truncated in its header"
            ));
        }
        let (payload_len, size_field_len) = if has_size_field {
            let (size, consumed) = read_leb128(&data[offset + header_len..])?;
            let size =
                usize::try_from(size).map_err(|_| "AV1 obu_size overflows usize".to_string())?;
            (size, consumed)
        } else {
            (data.len() - offset - header_len, 0)
        };
        let payload_start = offset + header_len + size_field_len;
        let payload_end = payload_start
            .checked_add(payload_len)
            .ok_or_else(|| "AV1 OBU payload length overflow".to_string())?;
        if payload_end > data.len() {
            return Err(format!(
                "AV1 OBU at offset {offset} claims {payload_len} payload bytes but only {} \
                 remain",
                data.len() - payload_start
            ));
        }
        obus.push(Obu {
            obu_type,
            whole: &data[offset..payload_end],
            payload: &data[payload_start..payload_end],
        });
        if !has_size_field {
            // Sanctioned only for the last OBU in a sample (av1-isobmff
            // `index.bs`: "it is assumed to fill the remainder of the
            // sample"), so `payload_end` is already `data.len()` here --
            // there is nothing left to walk.
            break;
        }
        offset = payload_end;
    }
    Ok(obus)
}

/// The `av1C` (`AV1CodecConfigurationRecord`, av1-isobmff `index.bs`
/// section 2.3.3) fields, plus the coded dimensions
/// `CMVideoFormatDescriptionCreate` needs, extracted from a Sequence
/// Header OBU's payload (av1-spec `sequence_header_obu()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceHeaderFields {
    pub seq_profile: u8,
    pub seq_level_idx_0: u8,
    pub seq_tier_0: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub chroma_subsampling_x: u8,
    pub chroma_subsampling_y: u8,
    pub chroma_sample_position: u8,
    pub max_frame_width: u32,
    pub max_frame_height: u32,
}

/// Parses a Sequence Header OBU payload (av1-spec
/// `06.bitstream.syntax.md`'s `sequence_header_obu()`/`color_config()`),
/// stopping as soon as every [`SequenceHeaderFields`] value has been
/// read -- `separate_uv_delta_q`, `film_grain_params_present` and
/// anything after them are never consulted, so they are never read
/// either (bit-reading is strictly sequential, so it is always correct
/// to stop once the fields a caller needs have all been read in the
/// exact order/width the spec lays out, which every field below is).
///
/// # Errors
///
/// Returns `Err` for a truncated buffer, or for
/// `timing_info_present_flag == 1` (see this module's doc).
#[allow(clippy::similar_names, clippy::too_many_lines)]
pub fn parse_sequence_header(payload: &[u8]) -> Result<SequenceHeaderFields, String> {
    let mut r = BitReader::new(payload);
    let seq_profile = r.f_u8(3)?;
    let _still_picture = r.f(1)?;
    let reduced_still_picture_header = r.f(1)? != 0;

    let mut seq_level_idx_0 = 0u8;
    let mut seq_tier_0 = 0u8;
    if reduced_still_picture_header {
        seq_level_idx_0 = r.f_u8(5)?;
        // seq_tier[0] = 0 (av1-spec).
    } else {
        let timing_info_present_flag = r.f(1)? != 0;
        if timing_info_present_flag {
            return Err("AV1 sequence header has timing_info_present_flag=1; \
                 timing_info()/decoder_model_info()/operating_parameters_info() are not \
                 implemented by this parser (irrelevant to a live remote-desktop stream \
                 with no container-level timing model)"
                .to_string());
        }
        // decoder_model_info_present_flag = 0 is forced by the spec
        // whenever timing_info_present_flag == 0 (the only case
        // supported above), so decoder_model_present_for_this_op[i] is
        // always 0 below and operating_parameters_info() is never
        // present -- no bits to read for it.
        let initial_display_delay_present_flag = r.f(1)? != 0;
        let operating_points_cnt_minus_1 = r.f(5)?;
        for i in 0..=operating_points_cnt_minus_1 {
            let _operating_point_idc = r.f(12)?;
            let seq_level_idx = r.f_u8(5)?;
            let seq_tier = if seq_level_idx > 7 { r.f_u8(1)? } else { 0 };
            if i == 0 {
                seq_level_idx_0 = seq_level_idx;
                seq_tier_0 = seq_tier;
            }
            if initial_display_delay_present_flag {
                let present_for_this_op = r.f(1)? != 0;
                if present_for_this_op {
                    let _initial_display_delay_minus_1 = r.f(4)?;
                }
            }
        }
    }

    let frame_width_bits_minus_1 = r.f(4)?;
    let frame_height_bits_minus_1 = r.f(4)?;
    let max_frame_width = r.f(frame_width_bits_minus_1 + 1)? + 1;
    let max_frame_height = r.f(frame_height_bits_minus_1 + 1)? + 1;

    let frame_id_numbers_present_flag = if reduced_still_picture_header {
        false
    } else {
        r.f(1)? != 0
    };
    if frame_id_numbers_present_flag {
        let _delta_frame_id_length_minus_2 = r.f(4)?;
        let _additional_frame_id_length_minus_1 = r.f(3)?;
    }

    let _use_128x128_superblock = r.f(1)?;
    let _enable_filter_intra = r.f(1)?;
    let _enable_intra_edge_filter = r.f(1)?;

    if !reduced_still_picture_header {
        let _enable_interintra_compound = r.f(1)?;
        let _enable_masked_compound = r.f(1)?;
        let _enable_warped_motion = r.f(1)?;
        let _enable_dual_filter = r.f(1)?;
        let enable_order_hint = r.f(1)? != 0;
        if enable_order_hint {
            let _enable_jnt_comp = r.f(1)?;
            let _enable_ref_frame_mvs = r.f(1)?;
        }
        let seq_choose_screen_content_tools = r.f(1)? != 0;
        // SELECT_SCREEN_CONTENT_TOOLS = 2 (av1-spec `03.symbols.md`):
        // always non-zero, so only whether the *explicit* value read
        // below is zero matters to the next `if`.
        let seq_force_screen_content_tools = if seq_choose_screen_content_tools {
            2
        } else {
            r.f(1)?
        };
        if seq_force_screen_content_tools > 0 {
            let seq_choose_integer_mv = r.f(1)? != 0;
            if !seq_choose_integer_mv {
                let _seq_force_integer_mv = r.f(1)?;
            }
        }
        if enable_order_hint {
            let _order_hint_bits_minus_1 = r.f(3)?;
        }
    }

    let _enable_superres = r.f(1)?;
    let _enable_cdef = r.f(1)?;
    let _enable_restoration = r.f(1)?;

    // color_config() (av1-spec `06.bitstream.syntax.md`).
    let high_bitdepth = r.f(1)? != 0;
    let (twelve_bit, bit_depth) = if seq_profile == 2 && high_bitdepth {
        let twelve_bit = r.f(1)? != 0;
        (twelve_bit, if twelve_bit { 12 } else { 10 })
    } else {
        (false, if high_bitdepth { 10 } else { 8 })
    };
    let monochrome = if seq_profile == 1 {
        false
    } else {
        r.f(1)? != 0
    };
    let color_description_present_flag = r.f(1)? != 0;
    let (color_primaries, transfer_characteristics, matrix_coefficients) =
        if color_description_present_flag {
            (r.f(8)?, r.f(8)?, r.f(8)?)
        } else {
            (2, 2, 2) // CP_UNSPECIFIED, TC_UNSPECIFIED, MC_UNSPECIFIED.
        };

    let (chroma_subsampling_x, chroma_subsampling_y, chroma_sample_position) = if monochrome {
        let _color_range = r.f(1)?;
        (1u8, 1u8, 0u8) // CSP_UNKNOWN.
    } else if color_primaries == 1 && transfer_characteristics == 13 && matrix_coefficients == 0 {
        // CP_BT_709 && TC_SRGB && MC_IDENTITY: implied full-range,
        // unsubsampled 4:4:4 (av1-spec `color_config()`).
        (0u8, 0u8, 0u8)
    } else {
        let _color_range = r.f(1)?;
        let (subsampling_x, subsampling_y) = match seq_profile {
            0 => (1u8, 1u8),
            1 => (0u8, 0u8),
            _ if bit_depth == 12 => {
                let subsampling_x = r.f_u8(1)?;
                let subsampling_y = if subsampling_x != 0 { r.f_u8(1)? } else { 0 };
                (subsampling_x, subsampling_y)
            }
            _ => (1u8, 0u8),
        };
        let chroma_sample_position = if subsampling_x != 0 && subsampling_y != 0 {
            r.f_u8(2)?
        } else {
            0
        };
        (subsampling_x, subsampling_y, chroma_sample_position)
    };

    Ok(SequenceHeaderFields {
        seq_profile,
        seq_level_idx_0,
        seq_tier_0,
        high_bitdepth,
        twelve_bit,
        monochrome,
        chroma_subsampling_x,
        chroma_subsampling_y,
        chroma_sample_position,
        max_frame_width,
        max_frame_height,
    })
}

/// Builds the `av1C` box payload (`AV1CodecConfigurationRecord`,
/// av1-isobmff `index.bs` section 2.3.3): a fixed 4-byte header
/// followed verbatim by `sequence_header_obu` (the complete OBU --
/// header, `obu_size`, and payload -- exactly as it appeared on the
/// wire, per [`Obu::whole`]). `initial_presentation_delay_present` is
/// always signalled `0`: Arcen has no pre-buffered decode schedule to
/// describe (a live low-latency stream has no use for it), matching
/// the field's own semantics (a hint for how many samples to buffer
/// before starting presentation).
///
/// Matches the approach independently taken by Chromium, Firefox,
/// `WebKit` and the ALVR/Moonlight community `VideoToolbox` AV1 clients
/// (see this module's doc): `configOBUs` is the real OBU bytes, not a
/// re-serialisation of them.
#[must_use]
pub fn build_av1c(fields: &SequenceHeaderFields, sequence_header_obu: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(4 + sequence_header_obu.len());
    // marker(1)=1, version(7)=1.
    record.push(0x80 | 0x01);
    // seq_profile(3), seq_level_idx_0(5).
    record.push((fields.seq_profile << 5) | (fields.seq_level_idx_0 & 0x1f));
    // seq_tier_0(1), high_bitdepth(1), twelve_bit(1), monochrome(1),
    // chroma_subsampling_x(1), chroma_subsampling_y(1),
    // chroma_sample_position(2).
    record.push(
        ((fields.seq_tier_0 & 1) << 7)
            | (u8::from(fields.high_bitdepth) << 6)
            | (u8::from(fields.twelve_bit) << 5)
            | (u8::from(fields.monochrome) << 4)
            | ((fields.chroma_subsampling_x & 1) << 3)
            | ((fields.chroma_subsampling_y & 1) << 2)
            | (fields.chroma_sample_position & 0x03),
    );
    // reserved(3)=0, initial_presentation_delay_present(1)=0,
    // reserved(4)=0.
    record.push(0x00);
    record.extend_from_slice(sequence_header_obu);
    record
}

/// Renders a short `kind:length` summary of `obus`, for logging --
/// mirrors `summarize_nals`'s role for the Annex-B pipeline.
#[must_use]
pub fn summarize(obus: &[Obu<'_>]) -> String {
    let parts = obus
        .iter()
        .map(|obu| {
            let kind: std::borrow::Cow<'static, str> = match obu.obu_type {
                ObuType::SequenceHeader => "seq_header".into(),
                ObuType::TemporalDelimiter => "temporal_delimiter".into(),
                ObuType::FrameHeader => "frame_header".into(),
                ObuType::TileGroup => "tile_group".into(),
                ObuType::Metadata => "metadata".into(),
                ObuType::Frame => "frame".into(),
                ObuType::RedundantFrameHeader => "redundant_frame_header".into(),
                ObuType::TileList => "tile_list".into(),
                ObuType::Padding => "padding".into(),
                ObuType::Reserved(value) => format!("reserved({value})").into(),
            };
            format!("{kind}:{}", obu.whole.len())
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("obus=[{parts}]")
}

#[cfg(test)]
mod tests {
    use crate::obu as av1;

    #[test]
    fn parses_a_temporal_delimiter_then_a_sequence_header_obu() {
        // Hand-computed bytes (see inline comments for the exact bit
        // layout), independent of any writer: this exercises the OBU
        // framing layer itself (`av1::parse_obus`), not the
        // sequence-header field parser exercised by the round-trip tests
        // below.
        //
        // Temporal delimiter: obu_forbidden_bit=0, obu_type=2
        // (OBU_TEMPORAL_DELIMITER), obu_extension_flag=0,
        // obu_has_size_field=1, obu_reserved_1bit=0
        //   -> 0_0010_0_1_0 = 0x12; obu_size = leb128(0) = 0x00 (zero
        //   payload bytes -- av1-spec: "the temporal delimiter has an
        //   empty payload").
        // Sequence header: obu_type=1 (OBU_SEQUENCE_HEADER), otherwise
        // identical framing bits -> 0_0001_0_1_0 = 0x0A; obu_size =
        // leb128(1) = 0x01; one payload byte 0xAB (arbitrary content --
        // this test only checks OBU framing, not field decoding).
        let bytes = [0x12, 0x00, 0x0A, 0x01, 0xAB];
        let obus = av1::parse_obus(&bytes).expect("well-formed OBU stream");
        assert_eq!(obus.len(), 2);
        assert_eq!(obus[0].obu_type, av1::ObuType::TemporalDelimiter);
        assert_eq!(obus[0].whole, &bytes[0..2]);
        assert!(obus[0].payload.is_empty());
        assert_eq!(obus[1].obu_type, av1::ObuType::SequenceHeader);
        assert_eq!(obus[1].whole, &bytes[2..5]);
        assert_eq!(obus[1].payload, &[0xAB]);
        assert_eq!(
            av1::summarize(&obus),
            "obus=[temporal_delimiter:2,seq_header:3]"
        );
    }

    #[test]
    fn last_obu_may_omit_its_size_field_and_fills_the_remainder() {
        // Frame OBU (type 6) with obu_has_size_field=0: obu_forbidden_bit=0,
        // obu_type=0110, obu_extension_flag=0, obu_has_size_field=0,
        // obu_reserved_1bit=0 -> 0_0110_0_0_0 = 0x30. Per av1-isobmff's "AV1
        // Sample Format", this is only permitted for the last OBU in a
        // sample, whose payload then fills the rest of the buffer.
        let bytes = [0x30, 0xAB, 0xCD];
        let obus = av1::parse_obus(&bytes).expect("well-formed OBU stream");
        assert_eq!(obus.len(), 1);
        assert_eq!(obus[0].obu_type, av1::ObuType::Frame);
        assert_eq!(obus[0].payload, &[0xAB, 0xCD]);
    }

    #[test]
    fn parse_obus_rejects_forbidden_bit_and_truncation() {
        assert!(av1::parse_obus(&[0x80]).is_err(), "obu_forbidden_bit set");
        assert!(av1::parse_obus(&[]).unwrap().is_empty());
        // obu_has_size_field=1 but the leb128 byte is missing entirely.
        assert!(av1::parse_obus(&[0x0A]).is_err());
        // obu_size (5) claims more payload than the 1 byte that remains.
        assert!(av1::parse_obus(&[0x0A, 0x05, 0xAB]).is_err());
    }

    /// Big-endian, MSB-first bit writer mirroring [`av1`]'s private
    /// `BitReader`, test-only: builds synthetic Sequence Header OBU
    /// payloads to verify `av1::parse_sequence_header` extracts the exact
    /// fields that were written. Each test below writes every syntax
    /// element inline, in the exact order/width given by
    /// AOMediaCodec/av1-spec `06.bitstream.syntax.md`'s
    /// `sequence_header_obu()`/`color_config()` (the same tables
    /// `parse_sequence_header`'s own doc cites), rather than going through
    /// a shared "spec to bits" helper -- so a bug in one is not masked by
    /// the same bug in the other.
    #[derive(Default)]
    struct TestBitWriter {
        bytes: Vec<u8>,
        partial: u8,
        bit_pos: u8,
    }

    impl TestBitWriter {
        fn write_bit(&mut self, bit: bool) {
            self.partial = (self.partial << 1) | u8::from(bit);
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.bytes.push(self.partial);
                self.partial = 0;
                self.bit_pos = 0;
            }
        }

        /// `f(n)`: writes the low `n_bits` of `value`, most-significant-bit-first.
        fn f(&mut self, value: u32, n_bits: u32) {
            for i in (0..n_bits).rev() {
                self.write_bit((value >> i) & 1 != 0);
            }
        }

        /// Zero-pads to the next byte boundary and returns the bytes.
        fn into_bytes(mut self) -> Vec<u8> {
            while self.bit_pos != 0 {
                self.write_bit(false);
            }
            self.bytes
        }
    }

    #[test]
    fn parses_a_reduced_still_picture_sequence_header_with_default_color_description() {
        let mut w = TestBitWriter::default();
        w.f(0, 3); // seq_profile = 0 (Main)
        w.f(1, 1); // still_picture = 1
        w.f(1, 1); // reduced_still_picture_header = 1
        w.f(4, 5); // seq_level_idx[0] = 4 (seq_tier[0] forced 0)
        // reduced_still_picture_header skips timing/operating-point/frame-id fields entirely.
        w.f(10, 4); // frame_width_bits_minus_1 = 10 (n = 11 bits)
        w.f(10, 4); // frame_height_bits_minus_1 = 10 (n = 11 bits)
        w.f(1919, 11); // max_frame_width_minus_1 = 1919 -> width = 1920
        w.f(1079, 11); // max_frame_height_minus_1 = 1079 -> height = 1080
        // frame_id_numbers_present_flag is forced 0 by reduced_still_picture_header (not written).
        w.f(0, 1); // use_128x128_superblock
        w.f(0, 1); // enable_filter_intra
        w.f(0, 1); // enable_intra_edge_filter
        // reduced_still_picture_header skips the enable_interintra_compound..order_hint_bits block.
        w.f(0, 1); // enable_superres
        w.f(0, 1); // enable_cdef
        w.f(0, 1); // enable_restoration
        // color_config():
        w.f(0, 1); // high_bitdepth = 0 (8-bit)
        // seq_profile != 2, so no twelve_bit read.
        w.f(0, 1); // mono_chrome = 0 (seq_profile != 1, so this bit is read)
        w.f(0, 1); // color_description_present_flag = 0 -> CP/TC/MC default to UNSPECIFIED(2)
        // Not the BT.709/sRGB/Identity shortcut (primaries default to 2, not 1) -> general branch:
        w.f(1, 1); // color_range = 1 (value irrelevant to this parser)
        // seq_profile == 0 -> subsampling_x = subsampling_y = 1 (not read from the bitstream).
        w.f(2, 2); // chroma_sample_position = 2 (subsampling_x && subsampling_y are both 1, so this IS read)
        let payload = w.into_bytes();

        let fields = av1::parse_sequence_header(&payload).expect("valid payload");
        assert_eq!(fields.seq_profile, 0);
        assert_eq!(fields.seq_level_idx_0, 4);
        assert_eq!(fields.seq_tier_0, 0);
        assert!(!fields.high_bitdepth);
        assert!(!fields.twelve_bit);
        assert!(!fields.monochrome);
        assert_eq!(fields.chroma_subsampling_x, 1);
        assert_eq!(fields.chroma_subsampling_y, 1);
        assert_eq!(fields.chroma_sample_position, 2);
        assert_eq!(fields.max_frame_width, 1920);
        assert_eq!(fields.max_frame_height, 1080);
    }

    #[test]
    fn parses_a_non_reduced_sequence_header_with_two_operating_points_and_explicit_color_description()
     {
        let mut w = TestBitWriter::default();
        w.f(1, 3); // seq_profile = 1 (High)
        w.f(0, 1); // still_picture = 0
        w.f(0, 1); // reduced_still_picture_header = 0
        w.f(0, 1); // timing_info_present_flag = 0
        w.f(0, 1); // initial_display_delay_present_flag = 0
        w.f(1, 5); // operating_points_cnt_minus_1 = 1 (two operating points)
        // operating point 0:
        w.f(0, 12); // operating_point_idc[0]
        w.f(8, 5); // seq_level_idx[0] = 8 (> 7, so seq_tier[0] is read)
        w.f(1, 1); // seq_tier[0] = 1 (High tier)
        // decoder_model_info_present_flag is forced 0, so operating_parameters_info() never
        // appears; initial_display_delay_present_flag == 0, so nothing more for this point.
        // operating point 1:
        w.f(0, 12); // operating_point_idc[1]
        w.f(4, 5); // seq_level_idx[1] = 4 (<= 7, seq_tier[1] = 0, not read)
        w.f(11, 4); // frame_width_bits_minus_1 = 11 (n = 12 bits)
        w.f(11, 4); // frame_height_bits_minus_1 = 11 (n = 12 bits)
        w.f(3839, 12); // max_frame_width_minus_1 = 3839 -> width = 3840
        w.f(2159, 12); // max_frame_height_minus_1 = 2159 -> height = 2160
        w.f(0, 1); // frame_id_numbers_present_flag = 0 (explicitly read; not reduced)
        w.f(1, 1); // use_128x128_superblock
        w.f(1, 1); // enable_filter_intra
        w.f(0, 1); // enable_intra_edge_filter
        // Not reduced -> the full block:
        w.f(0, 1); // enable_interintra_compound
        w.f(0, 1); // enable_masked_compound
        w.f(0, 1); // enable_warped_motion
        w.f(0, 1); // enable_dual_filter
        w.f(1, 1); // enable_order_hint = 1
        w.f(0, 1); // enable_jnt_comp
        w.f(0, 1); // enable_ref_frame_mvs
        w.f(1, 1); // seq_choose_screen_content_tools = 1 -> SELECT (2), no explicit bit
        w.f(1, 1); // seq_choose_integer_mv = 1 -> SELECT, no explicit bit (read because 2 > 0)
        w.f(2, 3); // order_hint_bits_minus_1 = 2 (enable_order_hint == 1)
        w.f(1, 1); // enable_superres
        w.f(1, 1); // enable_cdef
        w.f(1, 1); // enable_restoration
        // color_config():
        w.f(1, 1); // high_bitdepth = 1
        // seq_profile == 1, not 2, so no twelve_bit read even though high_bitdepth is set.
        // seq_profile == 1 -> mono_chrome is NOT read (forced 0).
        w.f(1, 1); // color_description_present_flag = 1
        w.f(9, 8); // color_primaries = 9 (BT.2020)
        w.f(16, 8); // transfer_characteristics = 16 (PQ)
        w.f(9, 8); // matrix_coefficients = 9 (BT.2020 NCL)
        // Not the BT.709/sRGB/Identity shortcut (primaries=9) -> general branch:
        w.f(1, 1); // color_range = 1
        // seq_profile == 1 -> subsampling_x = subsampling_y = 0 (not read); both zero -> no
        // chroma_sample_position read either.
        let payload = w.into_bytes();

        let fields = av1::parse_sequence_header(&payload).expect("valid payload");
        assert_eq!(fields.seq_profile, 1);
        assert_eq!(fields.seq_level_idx_0, 8);
        assert_eq!(fields.seq_tier_0, 1);
        assert!(fields.high_bitdepth);
        assert!(!fields.twelve_bit);
        assert!(!fields.monochrome);
        assert_eq!(fields.chroma_subsampling_x, 0);
        assert_eq!(fields.chroma_subsampling_y, 0);
        assert_eq!(fields.chroma_sample_position, 0);
        assert_eq!(fields.max_frame_width, 3840);
        assert_eq!(fields.max_frame_height, 2160);
    }

    #[test]
    fn parses_a_monochrome_sequence_header() {
        let mut w = TestBitWriter::default();
        w.f(0, 3); // seq_profile = 0
        w.f(1, 1); // still_picture = 1
        w.f(1, 1); // reduced_still_picture_header = 1
        w.f(0, 5); // seq_level_idx[0] = 0
        w.f(5, 4); // frame_width_bits_minus_1 = 5 (n = 6 bits)
        w.f(5, 4); // frame_height_bits_minus_1 = 5 (n = 6 bits)
        w.f(63, 6); // max_frame_width_minus_1 = 63 -> width = 64
        w.f(63, 6); // max_frame_height_minus_1 = 63 -> height = 64
        w.f(0, 1); // use_128x128_superblock
        w.f(0, 1); // enable_filter_intra
        w.f(0, 1); // enable_intra_edge_filter
        w.f(0, 1); // enable_superres
        w.f(0, 1); // enable_cdef
        w.f(0, 1); // enable_restoration
        // color_config():
        w.f(0, 1); // high_bitdepth = 0
        w.f(1, 1); // mono_chrome = 1 (seq_profile != 1, so this bit is read)
        w.f(0, 1); // color_description_present_flag = 0
        // mono_chrome branch reads exactly one more bit (color_range) then returns:
        w.f(1, 1); // color_range (value irrelevant to this parser)
        let payload = w.into_bytes();

        let fields = av1::parse_sequence_header(&payload).expect("valid payload");
        assert!(fields.monochrome);
        assert_eq!(fields.chroma_subsampling_x, 1);
        assert_eq!(fields.chroma_subsampling_y, 1);
        assert_eq!(fields.chroma_sample_position, 0);
        assert_eq!(fields.max_frame_width, 64);
        assert_eq!(fields.max_frame_height, 64);
    }

    #[test]
    fn rejects_timing_info_present_flag() {
        let mut w = TestBitWriter::default();
        w.f(0, 3); // seq_profile
        w.f(0, 1); // still_picture
        w.f(0, 1); // reduced_still_picture_header = 0
        w.f(1, 1); // timing_info_present_flag = 1
        let payload = w.into_bytes();
        let error = av1::parse_sequence_header(&payload).expect_err("must reject timing info");
        assert!(error.contains("timing_info_present_flag"));
    }

    #[test]
    fn parse_sequence_header_rejects_a_truncated_payload() {
        assert!(av1::parse_sequence_header(&[]).is_err());
        // Eight bits is nowhere near enough for even the reduced branch.
        assert!(av1::parse_sequence_header(&[0x00]).is_err());
    }

    #[test]
    fn build_av1c_packs_the_header_bytes_exactly_per_the_isobmff_spec() {
        // AOMediaCodec/av1-isobmff `index.bs` section 2.3.3
        // (`AV1CodecConfigurationRecord`): marker(1)=1,version(7)=1 ->
        // 0x81; seq_profile(3)=0,seq_level_idx_0(5)=4 -> 0b000_00100 =
        // 0x04; seq_tier_0(1)=0,high_bitdepth(1)=0,twelve_bit(1)=0,
        // monochrome(1)=0,ss_x(1)=1,ss_y(1)=1,csp(2)=0b00 ->
        // 0b0_0_0_0_1_1_00 = 0x0C; reserved/no-delay byte -> 0x00.
        let fields = av1::SequenceHeaderFields {
            seq_profile: 0,
            seq_level_idx_0: 4,
            seq_tier_0: 0,
            high_bitdepth: false,
            twelve_bit: false,
            monochrome: false,
            chroma_subsampling_x: 1,
            chroma_subsampling_y: 1,
            chroma_sample_position: 0,
            max_frame_width: 1920,
            max_frame_height: 1080,
        };
        let sequence_header_obu = [0xAA, 0xBB, 0xCC];
        let record = av1::build_av1c(&fields, &sequence_header_obu);
        assert_eq!(record, vec![0x81, 0x04, 0x0C, 0x00, 0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn build_av1c_packs_a_second_field_combination_correctly() {
        // seq_profile(3)=2,seq_level_idx_0(5)=31 -> 0b010_11111 = 0x5F;
        // seq_tier_0(1)=1,high_bitdepth(1)=1,twelve_bit(1)=0,
        // monochrome(1)=1,ss_x(1)=1,ss_y(1)=1,csp(2)=0b11 ->
        // 0b1_1_0_1_1_1_11 = 0xDF.
        let fields = av1::SequenceHeaderFields {
            seq_profile: 2,
            seq_level_idx_0: 31,
            seq_tier_0: 1,
            high_bitdepth: true,
            twelve_bit: false,
            monochrome: true,
            chroma_subsampling_x: 1,
            chroma_subsampling_y: 1,
            chroma_sample_position: 3,
            max_frame_width: 64,
            max_frame_height: 64,
        };
        let sequence_header_obu = [0x01, 0x02];
        let record = av1::build_av1c(&fields, &sequence_header_obu);
        assert_eq!(record, vec![0x81, 0x5F, 0xDF, 0x00, 0x01, 0x02]);
        // configOBUs is the input slice, byte-for-byte, never re-serialised.
        assert_eq!(&record[4..], &sequence_header_obu);
    }
}
