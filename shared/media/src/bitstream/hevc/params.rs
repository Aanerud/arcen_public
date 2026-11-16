// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! H.265 parameter sets (ITU-T H.265 §7.3.2): VPS, SPS and PPS, including
//! the format range extensions (4:4:4, high bit depth) and the VUI/HRD
//! syntax that sits in front of them.

use super::super::picture::{ColourDescription, ceil_log2};
use super::super::{BitReader, BitstreamError};

/// The most sub-layers any H.265 stream may have.
pub const MAX_SUB_LAYERS: usize = 7;
/// Largest decoded picture buffer H.265 allows (`MaxDpbSize`).
pub const MAX_DPB_SIZE: u32 = 16;

/// `profile_tier_level()` general fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProfileTierLevel {
    pub profile_space: u8,
    pub tier_flag: bool,
    pub profile_idc: u8,
    pub compatibility_flags: u32,
    pub level_idc: u8,
}

fn profile_tier_level(
    reader: &mut BitReader<'_>,
    max_sub_layers_minus1: u32,
) -> Result<ProfileTierLevel, BitstreamError> {
    let profile_space = reader.u8(2)?;
    let tier_flag = reader.flag()?;
    let profile_idc = reader.u8(5)?;
    let compatibility_flags = reader.u(32)?;
    // progressive, interlaced, non-packed and frame-only source flags, the
    // 43 constraint/reserved bits and the inbld/reserved bit.
    reader.skip(4 + 43 + 1)?;
    let level_idc = reader.u8(8)?;
    let mut profile_present = [false; MAX_SUB_LAYERS];
    let mut level_present = [false; MAX_SUB_LAYERS];
    let layers = max_sub_layers_minus1 as usize;
    for index in 0..layers {
        profile_present[index] = reader.flag()?;
        level_present[index] = reader.flag()?;
    }
    if layers > 0 {
        reader.skip(2 * (8 - layers))?;
    }
    for index in 0..layers {
        if profile_present[index] {
            reader.skip(88)?;
        }
        if level_present[index] {
            reader.skip(8)?;
        }
    }
    Ok(ProfileTierLevel {
        profile_space,
        tier_flag,
        profile_idc,
        compatibility_flags,
        level_idc,
    })
}

/// Per-sub-layer DPB sizing (`sps_max_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubLayerOrdering {
    pub max_dec_pic_buffering_minus1: u8,
    pub max_num_reorder_pics: u8,
    pub max_latency_increase_plus1: u32,
}

fn sub_layer_ordering(
    reader: &mut BitReader<'_>,
    max_sub_layers_minus1: u32,
) -> Result<[SubLayerOrdering; MAX_SUB_LAYERS], BitstreamError> {
    let present = reader.flag()?;
    let mut table = [SubLayerOrdering::default(); MAX_SUB_LAYERS];
    let first = if present { 0 } else { max_sub_layers_minus1 };
    for index in first..=max_sub_layers_minus1 {
        let buffering = reader.ue_max(MAX_DPB_SIZE - 1, "max_dec_pic_buffering_minus1")?;
        let reorder = reader.ue_max(buffering, "max_num_reorder_pics")?;
        let latency = reader.ue_max(u32::MAX - 1, "max_latency_increase_plus1")?;
        table[index as usize] = SubLayerOrdering {
            max_dec_pic_buffering_minus1: narrow(buffering),
            max_num_reorder_pics: narrow(reorder),
            max_latency_increase_plus1: latency,
        };
    }
    if !present {
        let top = table[max_sub_layers_minus1 as usize];
        for entry in table.iter_mut().take(max_sub_layers_minus1 as usize) {
            *entry = top;
        }
    }
    Ok(table)
}

/// A video parameter set: only what decoding a single-layer stream checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vps {
    pub id: u8,
    pub max_sub_layers_minus1: u8,
    pub profile_tier_level: ProfileTierLevel,
}

impl Vps {
    /// Parses a VPS RBSP (after the two-byte NAL header).
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] on truncation or out-of-range fields.
    pub fn parse(rbsp: &[u8]) -> Result<Self, BitstreamError> {
        let mut reader = BitReader::new(rbsp);
        let id = reader.u8(4)?;
        reader.skip(2)?; // vps_base_layer_internal_flag, vps_base_layer_available_flag
        let _max_layers_minus1 = reader.u(6)?;
        let max_sub_layers_minus1 = reader.u(3)?;
        if max_sub_layers_minus1 as usize >= MAX_SUB_LAYERS {
            return Err(BitstreamError::Invalid("vps_max_sub_layers_minus1"));
        }
        reader.skip(1)?; // vps_temporal_id_nesting_flag
        if reader.u(16)? != 0xffff {
            return Err(BitstreamError::Invalid("vps_reserved_0xffff_16bits"));
        }
        let profile_tier_level = profile_tier_level(&mut reader, max_sub_layers_minus1)?;
        sub_layer_ordering(&mut reader, max_sub_layers_minus1)?;
        Ok(Self {
            id,
            max_sub_layers_minus1: narrow(max_sub_layers_minus1),
            profile_tier_level,
        })
    }
}

/// Scaling lists (`scaling_list_data()`), in coded (up-right diagonal) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingList {
    /// 4×4 lists, matrixId 0..6.
    pub size4: [[u8; 16]; 6],
    /// 8×8 lists, matrixId 0..6.
    pub size8: [[u8; 64]; 6],
    /// 16×16 lists (8×8 coefficients upsampled by the decoder), matrixId 0..6.
    pub size16: [[u8; 64]; 6],
    /// 32×32 lists, matrixId 0..6 (only 0 and 3 are coded; for 4:4:4 the
    /// chroma ones are derived from the 16×16 lists).
    pub size32: [[u8; 64]; 6],
    pub dc16: [u8; 6],
    pub dc32: [u8; 6],
}

/// Table 7-6, intra 8×8 and larger, in coded order.
const DEFAULT_INTRA: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19, 20,
    21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31,
    29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];
/// Table 7-6, inter 8×8 and larger, in coded order.
const DEFAULT_INTER: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20, 20,
    20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28,
    28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

impl Default for ScalingList {
    /// The default lists (Tables 7-5 and 7-6), used when
    /// `scaling_list_enabled_flag` is set without explicit data.
    fn default() -> Self {
        let large = |matrix: usize| {
            if matrix < 3 {
                DEFAULT_INTRA
            } else {
                DEFAULT_INTER
            }
        };
        Self {
            size4: [[16; 16]; 6],
            size8: std::array::from_fn(large),
            size16: std::array::from_fn(large),
            size32: std::array::from_fn(large),
            dc16: [16; 6],
            dc32: [16; 6],
        }
    }
}

impl ScalingList {
    /// A flat list (every factor 16): what decoding uses when scaling lists
    /// are disabled.
    #[must_use]
    pub const fn flat() -> Self {
        Self {
            size4: [[16; 16]; 6],
            size8: [[16; 64]; 6],
            size16: [[16; 64]; 6],
            size32: [[16; 64]; 6],
            dc16: [16; 6],
            dc32: [16; 6],
        }
    }

    fn list_mut(&mut self, size_id: usize, matrix: usize) -> &mut [u8] {
        match size_id {
            0 => &mut self.size4[matrix],
            1 => &mut self.size8[matrix],
            2 => &mut self.size16[matrix],
            _ => &mut self.size32[matrix],
        }
    }

    fn list(&self, size_id: usize, matrix: usize) -> &[u8] {
        match size_id {
            0 => &self.size4[matrix],
            1 => &self.size8[matrix],
            2 => &self.size16[matrix],
            _ => &self.size32[matrix],
        }
    }

    fn set_dc(&mut self, size_id: usize, matrix: usize, value: u8) {
        match size_id {
            2 => self.dc16[matrix] = value,
            3 => self.dc32[matrix] = value,
            _ => {}
        }
    }

    fn dc(&self, size_id: usize, matrix: usize) -> u8 {
        match size_id {
            2 => self.dc16[matrix],
            3 => self.dc32[matrix],
            _ => 16,
        }
    }

    /// `scaling_list_data()` (§7.3.4).
    fn parse(reader: &mut BitReader<'_>, chroma_444: bool) -> Result<Self, BitstreamError> {
        let mut lists = Self::default();
        for size_id in 0..4_usize {
            let step = if size_id == 3 { 3 } else { 1 };
            let mut matrix = 0_usize;
            while matrix < 6 {
                let coefficients = if size_id == 0 { 16 } else { 64 };
                if reader.flag()? {
                    let mut next = 8_i32;
                    if size_id > 1 {
                        next = reader.se_range(-7, 247, "scaling_list_dc_coef_minus8")? + 8;
                        lists.set_dc(size_id, matrix, narrow_i(next));
                    }
                    for index in 0..coefficients {
                        let delta = reader.se_range(-128, 127, "scaling_list_delta_coef")?;
                        next = (next + delta + 256).rem_euclid(256);
                        if next == 0 {
                            return Err(BitstreamError::Invalid("scaling list coefficient"));
                        }
                        lists.list_mut(size_id, matrix)[index] = narrow_i(next);
                    }
                } else {
                    let max_delta = if size_id == 3 { matrix / 3 } else { matrix };
                    let delta = reader
                        .ue_max(narrow_u32(max_delta), "scaling_list_pred_matrix_id_delta")?
                        as usize;
                    let (source, dc) = if delta == 0 {
                        let default = Self::default();
                        (default.list(size_id, matrix).to_vec(), 16)
                    } else {
                        let reference = matrix - delta * step;
                        (
                            lists.list(size_id, reference).to_vec(),
                            lists.dc(size_id, reference),
                        )
                    };
                    lists.list_mut(size_id, matrix).copy_from_slice(&source);
                    lists.set_dc(size_id, matrix, dc);
                }
                matrix += step;
            }
        }
        if chroma_444 {
            // §7.4.5: 32×32 chroma factors come from the 16×16 lists.
            for matrix in [1, 2, 4, 5] {
                lists.size32[matrix] = lists.size16[matrix];
                lists.dc32[matrix] = lists.dc16[matrix];
            }
        }
        Ok(lists)
    }
}

/// A short-term reference picture set (§7.3.7, derived per §7.4.8).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShortTermRps {
    /// `DeltaPocS0`, negative, nearest first.
    pub delta_poc_s0: Vec<i32>,
    pub used_s0: Vec<bool>,
    /// `DeltaPocS1`, positive, nearest first.
    pub delta_poc_s1: Vec<i32>,
    pub used_s1: Vec<bool>,
    /// For a set predicted from another, `NumDeltaPocs[RefRpsIdx]`
    /// (`ucNumDeltaPocsOfRefRpsIdx` in DXVA); 0 otherwise.
    pub num_delta_pocs_of_ref: u8,
}

impl ShortTermRps {
    #[must_use]
    pub fn num_delta_pocs(&self) -> usize {
        self.delta_poc_s0.len() + self.delta_poc_s1.len()
    }

    /// Pictures of this set that the current picture may reference.
    #[must_use]
    pub fn num_used(&self) -> usize {
        self.used_s0
            .iter()
            .chain(&self.used_s1)
            .filter(|used| **used)
            .count()
    }

    /// `st_ref_pic_set(stRpsIdx)`. `previous` holds the SPS's sets
    /// `0..stRpsIdx`; `in_slice_header` is true for the set coded in a slice
    /// header (`stRpsIdx == num_short_term_ref_pic_sets`).
    pub(crate) fn parse(
        reader: &mut BitReader<'_>,
        previous: &[Self],
        in_slice_header: bool,
        max_dec_pic_buffering_minus1: u32,
    ) -> Result<Self, BitstreamError> {
        let index = previous.len();
        let predicted = index != 0 && reader.flag()?;
        if !predicted {
            return Self::parse_explicit(reader, max_dec_pic_buffering_minus1);
        }
        let delta_idx = if in_slice_header {
            reader.ue_max(narrow_u32(index - 1), "delta_idx_minus1")? as usize + 1
        } else {
            1
        };
        let reference = &previous[index - delta_idx];
        let sign = reader.flag()?;
        let magnitude = i32::try_from(reader.ue_max(32767, "abs_delta_rps_minus1")? + 1)
            .map_err(|_| BitstreamError::Invalid("abs_delta_rps_minus1"))?;
        let delta_rps = if sign { -magnitude } else { magnitude };
        let count = reference.num_delta_pocs();
        let mut used = Vec::with_capacity(count + 1);
        let mut use_delta = Vec::with_capacity(count + 1);
        for _ in 0..=count {
            let used_flag = reader.flag()?;
            used.push(used_flag);
            use_delta.push(used_flag || reader.flag()?);
        }
        let negatives = reference.delta_poc_s0.len();
        let mut set = Self {
            num_delta_pocs_of_ref: narrow(narrow_u32(count)),
            ..Self::default()
        };
        // (7-61): the new negative pictures, nearest first.
        for (j, delta) in reference.delta_poc_s1.iter().enumerate().rev() {
            let poc = delta + delta_rps;
            if poc < 0 && use_delta[negatives + j] {
                set.delta_poc_s0.push(poc);
                set.used_s0.push(used[negatives + j]);
            }
        }
        if delta_rps < 0 && use_delta[count] {
            set.delta_poc_s0.push(delta_rps);
            set.used_s0.push(used[count]);
        }
        for (j, delta) in reference.delta_poc_s0.iter().enumerate() {
            let poc = delta + delta_rps;
            if poc < 0 && use_delta[j] {
                set.delta_poc_s0.push(poc);
                set.used_s0.push(used[j]);
            }
        }
        // (7-62): the new positive pictures, nearest first.
        for (j, delta) in reference.delta_poc_s0.iter().enumerate().rev() {
            let poc = delta + delta_rps;
            if poc > 0 && use_delta[j] {
                set.delta_poc_s1.push(poc);
                set.used_s1.push(used[j]);
            }
        }
        if delta_rps > 0 && use_delta[count] {
            set.delta_poc_s1.push(delta_rps);
            set.used_s1.push(used[count]);
        }
        for (j, delta) in reference.delta_poc_s1.iter().enumerate() {
            let poc = delta + delta_rps;
            if poc > 0 && use_delta[negatives + j] {
                set.delta_poc_s1.push(poc);
                set.used_s1.push(used[negatives + j]);
            }
        }
        if set.num_delta_pocs() > MAX_DPB_SIZE as usize {
            return Err(BitstreamError::Invalid("short-term RPS size"));
        }
        Ok(set)
    }

    fn parse_explicit(
        reader: &mut BitReader<'_>,
        max_dec_pic_buffering_minus1: u32,
    ) -> Result<Self, BitstreamError> {
        let negatives = reader.ue_max(max_dec_pic_buffering_minus1, "num_negative_pics")?;
        let positives = reader.ue_max(
            max_dec_pic_buffering_minus1 - negatives,
            "num_positive_pics",
        )?;
        let mut set = Self::default();
        let mut poc = 0_i32;
        for _ in 0..negatives {
            poc -= i32::try_from(reader.ue_max(32767, "delta_poc_s0_minus1")? + 1)
                .map_err(|_| BitstreamError::Invalid("delta_poc_s0_minus1"))?;
            set.delta_poc_s0.push(poc);
            set.used_s0.push(reader.flag()?);
        }
        poc = 0;
        for _ in 0..positives {
            poc += i32::try_from(reader.ue_max(32767, "delta_poc_s1_minus1")? + 1)
                .map_err(|_| BitstreamError::Invalid("delta_poc_s1_minus1"))?;
            set.delta_poc_s1.push(poc);
            set.used_s1.push(reader.flag()?);
        }
        Ok(set)
    }
}

/// VUI fields a decoder or presenter uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vui {
    pub colour: Option<ColourDescription>,
    pub field_seq_flag: bool,
    pub default_display_window: Option<(u32, u32, u32, u32)>,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
}

fn vui_parameters(
    reader: &mut BitReader<'_>,
    max_sub_layers_minus1: u32,
) -> Result<Vui, BitstreamError> {
    let mut vui = Vui::default();
    if reader.flag()? && reader.u(8)? == 255 {
        // aspect_ratio_idc == EXTENDED_SAR: sar_width, sar_height
        reader.skip(32)?;
    }
    if reader.flag()? {
        reader.skip(1)?; // overscan_appropriate_flag
    }
    if reader.flag()? {
        reader.skip(3)?; // video_format
        let full_range = reader.flag()?;
        vui.colour = Some(if reader.flag()? {
            ColourDescription {
                full_range,
                primaries: reader.u8(8)?,
                transfer: reader.u8(8)?,
                matrix: reader.u8(8)?,
            }
        } else {
            ColourDescription {
                full_range,
                primaries: 2,
                transfer: 2,
                matrix: 2,
            }
        });
    }
    if reader.flag()? {
        reader.ue_max(5, "chroma_sample_loc_type_top_field")?;
        reader.ue_max(5, "chroma_sample_loc_type_bottom_field")?;
    }
    reader.skip(1)?; // neutral_chroma_indication_flag
    vui.field_seq_flag = reader.flag()?;
    reader.skip(1)?; // frame_field_info_present_flag
    if reader.flag()? {
        vui.default_display_window = Some((reader.ue()?, reader.ue()?, reader.ue()?, reader.ue()?));
    }
    if reader.flag()? {
        vui.num_units_in_tick = reader.u(32)?;
        vui.time_scale = reader.u(32)?;
        if reader.flag()? {
            reader.ue()?; // vui_num_ticks_poc_diff_one_minus1
        }
        if reader.flag()? {
            hrd_parameters(reader, true, max_sub_layers_minus1)?;
        }
    }
    if reader.flag()? {
        // bitstream_restriction_flag: tiles_fixed_structure,
        // motion_vectors_over_pic_boundaries, restricted_ref_pic_lists
        reader.skip(3)?;
        reader.ue_max(4095, "min_spatial_segmentation_idc")?;
        reader.ue_max(16, "max_bytes_per_pic_denom")?;
        reader.ue_max(16, "max_bits_per_min_cu_denom")?;
        reader.ue_max(15, "log2_max_mv_length_horizontal")?;
        reader.ue_max(15, "log2_max_mv_length_vertical")?;
    }
    Ok(vui)
}

/// `hrd_parameters()` (§E.2.2): read only to reach what follows the VUI.
pub(crate) fn hrd_parameters(
    reader: &mut BitReader<'_>,
    common_info: bool,
    max_sub_layers_minus1: u32,
) -> Result<(), BitstreamError> {
    let mut nal = false;
    let mut vcl = false;
    let mut sub_pic = false;
    if common_info {
        nal = reader.flag()?;
        vcl = reader.flag()?;
        if nal || vcl {
            sub_pic = reader.flag()?;
            if sub_pic {
                reader.skip(8 + 5 + 1 + 5)?;
            }
            reader.skip(4 + 4)?; // bit_rate_scale, cpb_size_scale
            if sub_pic {
                reader.skip(4)?; // cpb_size_du_scale
            }
            reader.skip(5 + 5 + 5)?;
        }
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_general = reader.flag()?;
        let fixed_within_cvs = fixed_general || reader.flag()?;
        let low_delay = if fixed_within_cvs {
            reader.ue_max(2047, "elemental_duration_in_tc_minus1")?;
            false
        } else {
            reader.flag()?
        };
        let cpb_count = if low_delay {
            1
        } else {
            reader.ue_max(31, "cpb_cnt_minus1")? + 1
        };
        for present in [nal, vcl] {
            if present {
                for _ in 0..cpb_count {
                    reader.ue()?; // bit_rate_value_minus1
                    reader.ue()?; // cpb_size_value_minus1
                    if sub_pic {
                        reader.ue()?; // cpb_size_du_value_minus1
                        reader.ue()?; // bit_rate_du_value_minus1
                    }
                    reader.skip(1)?; // cbr_flag
                }
            }
        }
    }
    Ok(())
}

/// `sps_range_extension()` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SpsRangeExtension {
    pub transform_skip_rotation_enabled_flag: bool,
    pub transform_skip_context_enabled_flag: bool,
    pub implicit_rdpcm_enabled_flag: bool,
    pub explicit_rdpcm_enabled_flag: bool,
    pub extended_precision_processing_flag: bool,
    pub intra_smoothing_disabled_flag: bool,
    pub high_precision_offsets_enabled_flag: bool,
    pub persistent_rice_adaptation_enabled_flag: bool,
    pub cabac_bypass_alignment_enabled_flag: bool,
}

/// PCM coding parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pcm {
    pub sample_bit_depth_luma_minus1: u8,
    pub sample_bit_depth_chroma_minus1: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub loop_filter_disabled_flag: bool,
}

/// A sequence parameter set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Sps {
    pub vps_id: u8,
    pub id: u8,
    pub max_sub_layers_minus1: u8,
    pub profile_tier_level: ProfileTierLevel,
    pub chroma_format_idc: u8,
    pub separate_colour_plane_flag: bool,
    pub pic_width_in_luma_samples: u32,
    pub pic_height_in_luma_samples: u32,
    /// Conformance window in luma samples: left, right, top, bottom.
    pub conformance_window: (u32, u32, u32, u32),
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub log2_max_pic_order_cnt_lsb: u8,
    pub sub_layer_ordering: [SubLayerOrdering; MAX_SUB_LAYERS],
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_luma_transform_block_size_minus2: u8,
    pub log2_diff_max_min_luma_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled_flag: bool,
    /// The SPS scaling lists (explicit or default) when enabled.
    pub scaling_list: Option<ScalingList>,
    pub amp_enabled_flag: bool,
    pub sample_adaptive_offset_enabled_flag: bool,
    pub pcm: Option<Pcm>,
    pub short_term_ref_pic_sets: Vec<ShortTermRps>,
    pub long_term_ref_pics_present_flag: bool,
    /// `lt_ref_pic_poc_lsb_sps` with `used_by_curr_pic_lt_sps_flag`.
    pub long_term_ref_pics_sps: Vec<(u32, bool)>,
    pub sps_temporal_mvp_enabled_flag: bool,
    pub strong_intra_smoothing_enabled_flag: bool,
    pub vui: Option<Vui>,
    pub range_extension: SpsRangeExtension,
}

impl Sps {
    /// Parses an SPS RBSP (after the two-byte NAL header).
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] on truncation, out-of-range fields, or the
    /// multilayer, 3D and screen-content extensions (not used by Arcen).
    #[allow(clippy::too_many_lines)]
    pub fn parse(rbsp: &[u8]) -> Result<Self, BitstreamError> {
        let mut reader = BitReader::new(rbsp);
        let vps_id = reader.u8(4)?;
        let max_sub_layers_minus1 = reader.u(3)?;
        if max_sub_layers_minus1 as usize >= MAX_SUB_LAYERS {
            return Err(BitstreamError::Invalid("sps_max_sub_layers_minus1"));
        }
        reader.skip(1)?; // sps_temporal_id_nesting_flag
        let profile_tier_level = profile_tier_level(&mut reader, max_sub_layers_minus1)?;
        let id = narrow(reader.ue_max(15, "sps_seq_parameter_set_id")?);
        let chroma_format_idc = narrow(reader.ue_max(3, "chroma_format_idc")?);
        let separate_colour_plane_flag = chroma_format_idc == 3 && reader.flag()?;
        let pic_width_in_luma_samples = reader.ue_max(16888, "pic_width_in_luma_samples")?;
        let pic_height_in_luma_samples = reader.ue_max(16888, "pic_height_in_luma_samples")?;
        if pic_width_in_luma_samples == 0 || pic_height_in_luma_samples == 0 {
            return Err(BitstreamError::Invalid("picture size"));
        }
        let (sub_width, sub_height) = match (chroma_format_idc, separate_colour_plane_flag) {
            (1, false) => (2, 2),
            (2, false) => (2, 1),
            _ => (1, 1),
        };
        let conformance_window = if reader.flag()? {
            let left = reader.ue()?.saturating_mul(sub_width);
            let right = reader.ue()?.saturating_mul(sub_width);
            let top = reader.ue()?.saturating_mul(sub_height);
            let bottom = reader.ue()?.saturating_mul(sub_height);
            if left.saturating_add(right) >= pic_width_in_luma_samples
                || top.saturating_add(bottom) >= pic_height_in_luma_samples
            {
                return Err(BitstreamError::Invalid("conformance_window"));
            }
            (left, right, top, bottom)
        } else {
            (0, 0, 0, 0)
        };
        let bit_depth_luma = narrow(reader.ue_max(8, "bit_depth_luma_minus8")? + 8);
        let bit_depth_chroma = narrow(reader.ue_max(8, "bit_depth_chroma_minus8")? + 8);
        let log2_max_pic_order_cnt_lsb =
            narrow(reader.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4);
        let sub_layer_ordering = sub_layer_ordering(&mut reader, max_sub_layers_minus1)?;
        let log2_min_cb_minus3 = reader.ue_max(3, "log2_min_luma_coding_block_size_minus3")?;
        let log2_diff_cb = reader.ue_max(3, "log2_diff_max_min_luma_coding_block_size")?;
        let ctb_log2 = log2_min_cb_minus3 + 3 + log2_diff_cb;
        if !(4..=6).contains(&ctb_log2) {
            return Err(BitstreamError::Invalid("CtbLog2SizeY"));
        }
        let log2_min_tb_minus2 = reader.ue_max(3, "log2_min_luma_transform_block_size_minus2")?;
        let log2_diff_tb = reader.ue_max(3, "log2_diff_max_min_luma_transform_block_size")?;
        if log2_min_tb_minus2 + 2 >= log2_min_cb_minus3 + 3
            || log2_min_tb_minus2 + 2 + log2_diff_tb > ctb_log2.min(5)
        {
            return Err(BitstreamError::Invalid("transform block size"));
        }
        let min_cb = 1_u32 << (log2_min_cb_minus3 + 3);
        if pic_width_in_luma_samples % min_cb != 0 || pic_height_in_luma_samples % min_cb != 0 {
            return Err(BitstreamError::Invalid("picture size"));
        }
        let max_depth = ctb_log2 - (log2_min_tb_minus2 + 2);
        let max_transform_hierarchy_depth_inter =
            narrow(reader.ue_max(max_depth, "max_transform_hierarchy_depth_inter")?);
        let max_transform_hierarchy_depth_intra =
            narrow(reader.ue_max(max_depth, "max_transform_hierarchy_depth_intra")?);
        let scaling_list_enabled_flag = reader.flag()?;
        let scaling_list = if scaling_list_enabled_flag {
            Some(if reader.flag()? {
                ScalingList::parse(&mut reader, chroma_format_idc == 3)?
            } else {
                ScalingList::default()
            })
        } else {
            None
        };
        let amp_enabled_flag = reader.flag()?;
        let sample_adaptive_offset_enabled_flag = reader.flag()?;
        let pcm = if reader.flag()? {
            Some(Pcm {
                sample_bit_depth_luma_minus1: reader.u8(4)?,
                sample_bit_depth_chroma_minus1: reader.u8(4)?,
                log2_min_pcm_luma_coding_block_size_minus3: narrow(
                    reader.ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")?,
                ),
                log2_diff_max_min_pcm_luma_coding_block_size: narrow(
                    reader.ue_max(3, "log2_diff_max_min_pcm_luma_coding_block_size")?,
                ),
                loop_filter_disabled_flag: reader.flag()?,
            })
        } else {
            None
        };
        let top_buffering = u32::from(
            sub_layer_ordering[max_sub_layers_minus1 as usize].max_dec_pic_buffering_minus1,
        );
        let num_sets = reader.ue_max(64, "num_short_term_ref_pic_sets")?;
        let mut short_term_ref_pic_sets: Vec<ShortTermRps> = Vec::with_capacity(num_sets as usize);
        for _ in 0..num_sets {
            let set =
                ShortTermRps::parse(&mut reader, &short_term_ref_pic_sets, false, top_buffering)?;
            short_term_ref_pic_sets.push(set);
        }
        let long_term_ref_pics_present_flag = reader.flag()?;
        let mut long_term_ref_pics_sps = Vec::new();
        if long_term_ref_pics_present_flag {
            let count = reader.ue_max(32, "num_long_term_ref_pics_sps")?;
            for _ in 0..count {
                let lsb = reader.u(u32::from(log2_max_pic_order_cnt_lsb))?;
                long_term_ref_pics_sps.push((lsb, reader.flag()?));
            }
        }
        let sps_temporal_mvp_enabled_flag = reader.flag()?;
        let strong_intra_smoothing_enabled_flag = reader.flag()?;
        let vui = if reader.flag()? {
            Some(vui_parameters(&mut reader, max_sub_layers_minus1)?)
        } else {
            None
        };
        let mut range_extension = SpsRangeExtension::default();
        if reader.flag()? {
            let range = reader.flag()?;
            let multilayer = reader.flag()?;
            let three_d = reader.flag()?;
            let scc = reader.flag()?;
            let _extension_4bits = reader.u(4)?;
            if range {
                range_extension = SpsRangeExtension {
                    transform_skip_rotation_enabled_flag: reader.flag()?,
                    transform_skip_context_enabled_flag: reader.flag()?,
                    implicit_rdpcm_enabled_flag: reader.flag()?,
                    explicit_rdpcm_enabled_flag: reader.flag()?,
                    extended_precision_processing_flag: reader.flag()?,
                    intra_smoothing_disabled_flag: reader.flag()?,
                    high_precision_offsets_enabled_flag: reader.flag()?,
                    persistent_rice_adaptation_enabled_flag: reader.flag()?,
                    cabac_bypass_alignment_enabled_flag: reader.flag()?,
                };
            }
            if multilayer || three_d || scc {
                return Err(BitstreamError::Unsupported(
                    "HEVC multilayer, 3D or screen-content SPS extension",
                ));
            }
        }
        Ok(Self {
            vps_id,
            id,
            max_sub_layers_minus1: narrow(max_sub_layers_minus1),
            profile_tier_level,
            chroma_format_idc,
            separate_colour_plane_flag,
            pic_width_in_luma_samples,
            pic_height_in_luma_samples,
            conformance_window,
            bit_depth_luma,
            bit_depth_chroma,
            log2_max_pic_order_cnt_lsb,
            sub_layer_ordering,
            log2_min_luma_coding_block_size_minus3: narrow(log2_min_cb_minus3),
            log2_diff_max_min_luma_coding_block_size: narrow(log2_diff_cb),
            log2_min_luma_transform_block_size_minus2: narrow(log2_min_tb_minus2),
            log2_diff_max_min_luma_transform_block_size: narrow(log2_diff_tb),
            max_transform_hierarchy_depth_inter,
            max_transform_hierarchy_depth_intra,
            scaling_list_enabled_flag,
            scaling_list,
            amp_enabled_flag,
            sample_adaptive_offset_enabled_flag,
            pcm,
            short_term_ref_pic_sets,
            long_term_ref_pics_present_flag,
            long_term_ref_pics_sps,
            sps_temporal_mvp_enabled_flag,
            strong_intra_smoothing_enabled_flag,
            vui,
            range_extension,
        })
    }

    /// `CtbLog2SizeY`.
    #[must_use]
    pub const fn ctb_log2_size(&self) -> u32 {
        self.log2_min_luma_coding_block_size_minus3 as u32
            + 3
            + self.log2_diff_max_min_luma_coding_block_size as u32
    }

    /// `PicWidthInCtbsY`.
    #[must_use]
    pub const fn pic_width_in_ctbs(&self) -> u32 {
        self.pic_width_in_luma_samples
            .div_ceil(1 << self.ctb_log2_size())
    }

    /// `PicHeightInCtbsY`.
    #[must_use]
    pub const fn pic_height_in_ctbs(&self) -> u32 {
        self.pic_height_in_luma_samples
            .div_ceil(1 << self.ctb_log2_size())
    }

    /// `PicWidthInMinCbsY`.
    #[must_use]
    pub const fn pic_width_in_min_cbs(&self) -> u32 {
        self.pic_width_in_luma_samples >> (self.log2_min_luma_coding_block_size_minus3 + 3)
    }

    /// `PicHeightInMinCbsY`.
    #[must_use]
    pub const fn pic_height_in_min_cbs(&self) -> u32 {
        self.pic_height_in_luma_samples >> (self.log2_min_luma_coding_block_size_minus3 + 3)
    }

    /// `ChromaArrayType`.
    #[must_use]
    pub const fn chroma_array_type(&self) -> u8 {
        if self.separate_colour_plane_flag {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// The ordering limits of the highest sub-layer (`HighestTid`).
    #[must_use]
    pub const fn highest_ordering(&self) -> SubLayerOrdering {
        self.sub_layer_ordering[self.max_sub_layers_minus1 as usize]
    }

    /// Bits of `slice_segment_address`.
    #[must_use]
    pub const fn slice_segment_address_bits(&self) -> u32 {
        ceil_log2(self.pic_width_in_ctbs() * self.pic_height_in_ctbs())
    }
}

/// `pps_range_extension()`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PpsRangeExtension {
    pub log2_max_transform_skip_block_size_minus2: u8,
    pub cross_component_prediction_enabled_flag: bool,
    pub chroma_qp_offset_list_enabled_flag: bool,
    pub diff_cu_chroma_qp_offset_depth: u8,
    pub chroma_qp_offset_list_len_minus1: u8,
    pub cb_qp_offset_list: [i8; 6],
    pub cr_qp_offset_list: [i8; 6],
    pub log2_sao_offset_scale_luma: u8,
    pub log2_sao_offset_scale_chroma: u8,
}

/// Tile layout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tiles {
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub uniform_spacing_flag: bool,
    pub column_width_minus1: Vec<u16>,
    pub row_height_minus1: Vec<u16>,
    pub loop_filter_across_tiles_enabled_flag: bool,
}

impl Tiles {
    /// Every column width and row height in CTBs, minus one (§6.5.1), with
    /// uniform spacing resolved against `sps`.
    ///
    /// # Errors
    ///
    /// [`BitstreamError::Invalid`] when the sizes do not fit the picture.
    pub fn resolve(&self, sps: &Sps) -> Result<(Vec<u16>, Vec<u16>), BitstreamError> {
        let resolve = |count: u32, total: u32, explicit: &[u16]| {
            let mut sizes = Vec::with_capacity(count as usize);
            if self.uniform_spacing_flag {
                for index in 0..count {
                    let size = ((index + 1) * total) / count - (index * total) / count;
                    if size == 0 {
                        return Err(BitstreamError::Invalid("tile size"));
                    }
                    sizes.push(narrow_u16(size - 1));
                }
            } else {
                let mut used = 0_u32;
                for size in explicit {
                    used += u32::from(*size) + 1;
                    sizes.push(*size);
                }
                if used >= total {
                    return Err(BitstreamError::Invalid("tile size"));
                }
                sizes.push(narrow_u16(total - used - 1));
            }
            Ok(sizes)
        };
        Ok((
            resolve(
                u32::from(self.num_tile_columns_minus1) + 1,
                sps.pic_width_in_ctbs(),
                &self.column_width_minus1,
            )?,
            resolve(
                u32::from(self.num_tile_rows_minus1) + 1,
                sps.pic_height_in_ctbs(),
                &self.row_height_minus1,
            )?,
        ))
    }
}

/// A picture parameter set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments_enabled_flag: bool,
    pub output_flag_present_flag: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding_enabled_flag: bool,
    pub cabac_init_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub constrained_intra_pred_flag: bool,
    pub transform_skip_enabled_flag: bool,
    pub cu_qp_delta_enabled_flag: bool,
    pub diff_cu_qp_delta_depth: u8,
    pub cb_qp_offset: i8,
    pub cr_qp_offset: i8,
    pub slice_chroma_qp_offsets_present_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_flag: bool,
    pub transquant_bypass_enabled_flag: bool,
    pub tiles: Option<Tiles>,
    pub entropy_coding_sync_enabled_flag: bool,
    pub loop_filter_across_slices_enabled_flag: bool,
    pub deblocking_filter_control_present_flag: bool,
    pub deblocking_filter_override_enabled_flag: bool,
    pub pps_deblocking_filter_disabled_flag: bool,
    pub beta_offset_div2: i8,
    pub tc_offset_div2: i8,
    /// Explicit PPS scaling lists, when present.
    pub scaling_list: Option<ScalingList>,
    pub lists_modification_present_flag: bool,
    pub log2_parallel_merge_level_minus2: u8,
    pub slice_segment_header_extension_present_flag: bool,
    pub range_extension: PpsRangeExtension,
}

impl Pps {
    /// Parses a PPS RBSP (after the two-byte NAL header). `chroma_444`
    /// (from the referenced SPS, when known) only affects how 32×32 chroma
    /// scaling lists are derived.
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] on truncation, out-of-range fields, or the
    /// multilayer, 3D and screen-content extensions.
    #[allow(clippy::too_many_lines)]
    pub fn parse(rbsp: &[u8], chroma_444: bool) -> Result<Self, BitstreamError> {
        let mut reader = BitReader::new(rbsp);
        let id = narrow(reader.ue_max(63, "pps_pic_parameter_set_id")?);
        let sps_id = narrow(reader.ue_max(15, "pps_seq_parameter_set_id")?);
        let dependent_slice_segments_enabled_flag = reader.flag()?;
        let output_flag_present_flag = reader.flag()?;
        let num_extra_slice_header_bits = reader.u8(3)?;
        let sign_data_hiding_enabled_flag = reader.flag()?;
        let cabac_init_present_flag = reader.flag()?;
        let num_ref_idx_l0_default_active_minus1 =
            narrow(reader.ue_max(14, "num_ref_idx_l0_default_active_minus1")?);
        let num_ref_idx_l1_default_active_minus1 =
            narrow(reader.ue_max(14, "num_ref_idx_l1_default_active_minus1")?);
        let init_qp_minus26 = narrow_i8(reader.se_range(-(26 + 48), 25, "init_qp_minus26")?);
        let constrained_intra_pred_flag = reader.flag()?;
        let transform_skip_enabled_flag = reader.flag()?;
        let cu_qp_delta_enabled_flag = reader.flag()?;
        let diff_cu_qp_delta_depth = if cu_qp_delta_enabled_flag {
            narrow(reader.ue_max(3, "diff_cu_qp_delta_depth")?)
        } else {
            0
        };
        let cb_qp_offset = narrow_i8(reader.se_range(-12, 12, "pps_cb_qp_offset")?);
        let cr_qp_offset = narrow_i8(reader.se_range(-12, 12, "pps_cr_qp_offset")?);
        let slice_chroma_qp_offsets_present_flag = reader.flag()?;
        let weighted_pred_flag = reader.flag()?;
        let weighted_bipred_flag = reader.flag()?;
        let transquant_bypass_enabled_flag = reader.flag()?;
        let tiles_enabled_flag = reader.flag()?;
        let entropy_coding_sync_enabled_flag = reader.flag()?;
        let tiles = if tiles_enabled_flag {
            // DXVA_PicParams_HEVC holds 19 column and 21 row widths.
            let columns = reader.ue_max(19, "num_tile_columns_minus1")?;
            let rows = reader.ue_max(21, "num_tile_rows_minus1")?;
            if columns == 0 && rows == 0 {
                return Err(BitstreamError::Invalid("tile count"));
            }
            let uniform_spacing_flag = reader.flag()?;
            let mut column_width_minus1 = Vec::new();
            let mut row_height_minus1 = Vec::new();
            if !uniform_spacing_flag {
                for _ in 0..columns {
                    column_width_minus1.push(narrow_u16(reader.ue_max(1056, "column_width")?));
                }
                for _ in 0..rows {
                    row_height_minus1.push(narrow_u16(reader.ue_max(1056, "row_height")?));
                }
            }
            Some(Tiles {
                num_tile_columns_minus1: narrow(columns),
                num_tile_rows_minus1: narrow(rows),
                uniform_spacing_flag,
                column_width_minus1,
                row_height_minus1,
                loop_filter_across_tiles_enabled_flag: reader.flag()?,
            })
        } else {
            None
        };
        let loop_filter_across_slices_enabled_flag = reader.flag()?;
        let deblocking_filter_control_present_flag = reader.flag()?;
        let mut deblocking_filter_override_enabled_flag = false;
        let mut pps_deblocking_filter_disabled_flag = false;
        let mut beta_offset_div2 = 0;
        let mut tc_offset_div2 = 0;
        if deblocking_filter_control_present_flag {
            deblocking_filter_override_enabled_flag = reader.flag()?;
            pps_deblocking_filter_disabled_flag = reader.flag()?;
            if !pps_deblocking_filter_disabled_flag {
                beta_offset_div2 = narrow_i8(reader.se_range(-6, 6, "pps_beta_offset_div2")?);
                tc_offset_div2 = narrow_i8(reader.se_range(-6, 6, "pps_tc_offset_div2")?);
            }
        }
        let scaling_list = if reader.flag()? {
            Some(ScalingList::parse(&mut reader, chroma_444)?)
        } else {
            None
        };
        let lists_modification_present_flag = reader.flag()?;
        let log2_parallel_merge_level_minus2 =
            narrow(reader.ue_max(4, "log2_parallel_merge_level_minus2")?);
        let slice_segment_header_extension_present_flag = reader.flag()?;
        let mut range_extension = PpsRangeExtension::default();
        if reader.flag()? {
            let range = reader.flag()?;
            let multilayer = reader.flag()?;
            let three_d = reader.flag()?;
            let scc = reader.flag()?;
            let _extension_4bits = reader.u(4)?;
            if range {
                if transform_skip_enabled_flag {
                    range_extension.log2_max_transform_skip_block_size_minus2 =
                        narrow(reader.ue_max(3, "log2_max_transform_skip_block_size_minus2")?);
                }
                range_extension.cross_component_prediction_enabled_flag = reader.flag()?;
                range_extension.chroma_qp_offset_list_enabled_flag = reader.flag()?;
                if range_extension.chroma_qp_offset_list_enabled_flag {
                    range_extension.diff_cu_chroma_qp_offset_depth =
                        narrow(reader.ue_max(3, "diff_cu_chroma_qp_offset_depth")?);
                    let length = reader.ue_max(5, "chroma_qp_offset_list_len_minus1")?;
                    range_extension.chroma_qp_offset_list_len_minus1 = narrow(length);
                    for index in 0..=length as usize {
                        range_extension.cb_qp_offset_list[index] =
                            narrow_i8(reader.se_range(-12, 12, "cb_qp_offset_list")?);
                        range_extension.cr_qp_offset_list[index] =
                            narrow_i8(reader.se_range(-12, 12, "cr_qp_offset_list")?);
                    }
                }
                range_extension.log2_sao_offset_scale_luma =
                    narrow(reader.ue_max(6, "log2_sao_offset_scale_luma")?);
                range_extension.log2_sao_offset_scale_chroma =
                    narrow(reader.ue_max(6, "log2_sao_offset_scale_chroma")?);
            }
            if multilayer || three_d || scc {
                return Err(BitstreamError::Unsupported(
                    "HEVC multilayer, 3D or screen-content PPS extension",
                ));
            }
        }
        Ok(Self {
            id,
            sps_id,
            dependent_slice_segments_enabled_flag,
            output_flag_present_flag,
            num_extra_slice_header_bits,
            sign_data_hiding_enabled_flag,
            cabac_init_present_flag,
            num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_default_active_minus1,
            init_qp_minus26,
            constrained_intra_pred_flag,
            transform_skip_enabled_flag,
            cu_qp_delta_enabled_flag,
            diff_cu_qp_delta_depth,
            cb_qp_offset,
            cr_qp_offset,
            slice_chroma_qp_offsets_present_flag,
            weighted_pred_flag,
            weighted_bipred_flag,
            transquant_bypass_enabled_flag,
            tiles,
            entropy_coding_sync_enabled_flag,
            loop_filter_across_slices_enabled_flag,
            deblocking_filter_control_present_flag,
            deblocking_filter_override_enabled_flag,
            pps_deblocking_filter_disabled_flag,
            beta_offset_div2,
            tc_offset_div2,
            scaling_list,
            lists_modification_present_flag,
            log2_parallel_merge_level_minus2,
            slice_segment_header_extension_present_flag,
            range_extension,
        })
    }

    /// Checks the PPS against its SPS once both are known (§7.4.3.3 ranges
    /// that depend on the SPS).
    ///
    /// # Errors
    ///
    /// [`BitstreamError::Invalid`] for an inconsistent pair.
    pub fn validate_against(&self, sps: &Sps) -> Result<(), BitstreamError> {
        let qp_bd_offset = 6 * (i32::from(sps.bit_depth_luma) - 8);
        if i32::from(self.init_qp_minus26) < -(26 + qp_bd_offset) {
            return Err(BitstreamError::Invalid("init_qp_minus26"));
        }
        if self.diff_cu_qp_delta_depth > sps.log2_diff_max_min_luma_coding_block_size {
            return Err(BitstreamError::Invalid("diff_cu_qp_delta_depth"));
        }
        if let Some(tiles) = &self.tiles {
            if u32::from(tiles.num_tile_columns_minus1) >= sps.pic_width_in_ctbs()
                || u32::from(tiles.num_tile_rows_minus1) >= sps.pic_height_in_ctbs()
            {
                return Err(BitstreamError::Invalid("tile count"));
            }
            tiles.resolve(sps)?;
        }
        Ok(())
    }
}

pub(crate) fn narrow(value: u32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

pub(crate) fn narrow_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

pub(crate) fn narrow_i8(value: i32) -> i8 {
    i8::try_from(value).unwrap_or(if value < 0 { i8::MIN } else { i8::MAX })
}

fn narrow_i(value: i32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

pub(crate) fn narrow_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
