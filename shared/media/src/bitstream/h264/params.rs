// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! H.264 parameter sets (ITU-T H.264 §7.3.2.1, §7.3.2.2) with their VUI and
//! scaling lists.

use super::super::picture::ColourDescription;
use super::super::{BitReader, BitstreamError};

/// `Flat_4x4_16` / `Flat_8x8_16`.
const FLAT: u8 = 16;

/// Table 7-3, `Default_4x4_Intra`, in coded (zig-zag) order.
const DEFAULT_4X4_INTRA: [u8; 16] = [
    6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
];
/// Table 7-3, `Default_4x4_Inter`.
const DEFAULT_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
];
/// Table 7-4, `Default_8x8_Intra`.
const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
/// Table 7-4, `Default_8x8_Inter`.
const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

/// Resolved scaling matrices, in coded (zig-zag) order: six 4×4 lists and
/// six 8×8 lists (Y/Cb/Cr intra, then inter; 8×8 chroma only for 4:4:4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingMatrix {
    pub lists4x4: [[u8; 16]; 6],
    pub lists8x8: [[u8; 64]; 6],
}

impl ScalingMatrix {
    /// Every factor 16.
    #[must_use]
    pub const fn flat() -> Self {
        Self {
            lists4x4: [[FLAT; 16]; 6],
            lists8x8: [[FLAT; 64]; 6],
        }
    }

    /// `scaling_list()` entries `0..count` with fall-back rule A (SPS,
    /// `base` `None`) or B (PPS, falling back to the SPS matrix `base`).
    /// Lists beyond `count` are not coded and follow the same fall-back.
    fn parse(
        reader: &mut BitReader<'_>,
        count: usize,
        base: Option<&Self>,
    ) -> Result<Self, BitstreamError> {
        let mut matrix = Self::flat();
        for index in 0..12 {
            let present = index < count && reader.flag()?;
            if index < 6 {
                let intra = index < 3;
                let default = if intra {
                    DEFAULT_4X4_INTRA
                } else {
                    DEFAULT_4X4_INTER
                };
                matrix.lists4x4[index] = if present {
                    read_list(reader)?.unwrap_or(default)
                } else {
                    match (index, base) {
                        (0 | 3, None) => default,
                        (0 | 3, Some(base)) => base.lists4x4[index],
                        _ => matrix.lists4x4[index - 1],
                    }
                };
            } else {
                let list = index - 6;
                let intra = list % 2 == 0;
                let default = if intra {
                    DEFAULT_8X8_INTRA
                } else {
                    DEFAULT_8X8_INTER
                };
                matrix.lists8x8[list] = if present {
                    read_list(reader)?.unwrap_or(default)
                } else {
                    match (list, base) {
                        (0 | 1, None) => default,
                        (0 | 1, Some(base)) => base.lists8x8[list],
                        _ => matrix.lists8x8[list - 2],
                    }
                };
            }
        }
        Ok(matrix)
    }
}

/// One `scaling_list()`; `None` when `useDefaultScalingMatrixFlag` is set.
fn read_list<const N: usize>(
    reader: &mut BitReader<'_>,
) -> Result<Option<[u8; N]>, BitstreamError> {
    let mut list = [0_u8; N];
    let mut last = 8_i32;
    let mut next = 8_i32;
    for (index, entry) in list.iter_mut().enumerate() {
        if next != 0 {
            let delta = reader.se_range(-128, 127, "delta_scale")?;
            next = (last + delta + 256).rem_euclid(256);
            if index == 0 && next == 0 {
                return Ok(None);
            }
        }
        let value = if next == 0 { last } else { next };
        *entry = u8::try_from(value).map_err(|_| BitstreamError::Invalid("scaling list"))?;
        last = value;
    }
    Ok(Some(list))
}

/// VUI fields a decoder uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vui {
    pub colour: Option<ColourDescription>,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    /// `max_num_reorder_frames`, when `bitstream_restriction_flag` is set.
    pub max_num_reorder_frames: Option<u8>,
    /// `max_dec_frame_buffering`, when `bitstream_restriction_flag` is set.
    pub max_dec_frame_buffering: Option<u8>,
}

fn hrd_parameters(reader: &mut BitReader<'_>) -> Result<(), BitstreamError> {
    let count = reader.ue_max(31, "cpb_cnt_minus1")? + 1;
    reader.skip(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..count {
        reader.ue()?; // bit_rate_value_minus1
        reader.ue()?; // cpb_size_value_minus1
        reader.skip(1)?; // cbr_flag
    }
    reader.skip(20)?; // four 5-bit delay/offset lengths
    Ok(())
}

fn vui_parameters(reader: &mut BitReader<'_>) -> Result<Vui, BitstreamError> {
    let mut vui = Vui::default();
    if reader.flag()? && reader.u(8)? == 255 {
        reader.skip(32)?; // sar_width, sar_height
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
    if reader.flag()? {
        vui.num_units_in_tick = reader.u(32)?;
        vui.time_scale = reader.u(32)?;
        reader.skip(1)?; // fixed_frame_rate_flag
    }
    let nal_hrd = reader.flag()?;
    if nal_hrd {
        hrd_parameters(reader)?;
    }
    let vcl_hrd = reader.flag()?;
    if vcl_hrd {
        hrd_parameters(reader)?;
    }
    if nal_hrd || vcl_hrd {
        reader.skip(1)?; // low_delay_hrd_flag
    }
    reader.skip(1)?; // pic_struct_present_flag
    if reader.flag()? {
        reader.skip(1)?; // motion_vectors_over_pic_boundaries_flag
        reader.ue_max(16, "max_bytes_per_pic_denom")?;
        reader.ue_max(16, "max_bits_per_mb_denom")?;
        reader.ue_max(16, "log2_max_mv_length_horizontal")?;
        reader.ue_max(16, "log2_max_mv_length_vertical")?;
        vui.max_num_reorder_frames = Some(narrow(reader.ue_max(16, "max_num_reorder_frames")?));
        vui.max_dec_frame_buffering = Some(narrow(reader.ue_max(16, "max_dec_frame_buffering")?));
    }
    Ok(vui)
}

/// Picture order count syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PocType {
    /// Type 0: `log2_max_pic_order_cnt_lsb`.
    Lsb { log2_max_lsb: u8 },
    /// Type 1: expected deltas per cycle.
    Cycle {
        delta_pic_order_always_zero_flag: bool,
        offset_for_non_ref_pic: i32,
        offset_for_top_to_bottom_field: i32,
        offset_for_ref_frame: Vec<i32>,
    },
    /// Type 2: output order equals decoding order.
    DecodingOrder,
}

impl PocType {
    /// `pic_order_cnt_type`.
    #[must_use]
    pub const fn id(&self) -> u8 {
        match self {
            Self::Lsb { .. } => 0,
            Self::Cycle { .. } => 1,
            Self::DecodingOrder => 2,
        }
    }
}

/// A sequence parameter set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub qpprime_y_zero_transform_bypass_flag: bool,
    /// The sequence scaling matrix (flat when not present).
    pub scaling_matrix: ScalingMatrix,
    pub seq_scaling_matrix_present_flag: bool,
    pub log2_max_frame_num: u8,
    pub poc_type: PocType,
    pub max_num_ref_frames: u8,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub pic_width_in_mbs: u32,
    pub pic_height_in_map_units: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    /// Frame cropping in luma samples: left, right, top, bottom.
    pub crop: (u32, u32, u32, u32),
    pub vui: Option<Vui>,
}

impl Sps {
    /// Parses an SPS RBSP (after the one-byte NAL header).
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] on truncation or out-of-range fields.
    #[allow(clippy::too_many_lines)]
    pub fn parse(rbsp: &[u8]) -> Result<Self, BitstreamError> {
        let mut reader = BitReader::new(rbsp);
        let profile_idc = reader.u8(8)?;
        let constraint_flags = reader.u8(8)?;
        let level_idc = reader.u8(8)?;
        let id = narrow(reader.ue_max(31, "seq_parameter_set_id")?);
        let mut chroma_format_idc = 1;
        let mut separate_colour_plane_flag = false;
        let mut bit_depth_luma = 8;
        let mut bit_depth_chroma = 8;
        let mut qpprime_y_zero_transform_bypass_flag = false;
        let mut seq_scaling_matrix_present_flag = false;
        let mut scaling_matrix = ScalingMatrix::flat();
        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            chroma_format_idc = narrow(reader.ue_max(3, "chroma_format_idc")?);
            if chroma_format_idc == 3 {
                separate_colour_plane_flag = reader.flag()?;
            }
            bit_depth_luma = narrow(reader.ue_max(6, "bit_depth_luma_minus8")? + 8);
            bit_depth_chroma = narrow(reader.ue_max(6, "bit_depth_chroma_minus8")? + 8);
            qpprime_y_zero_transform_bypass_flag = reader.flag()?;
            seq_scaling_matrix_present_flag = reader.flag()?;
            if seq_scaling_matrix_present_flag {
                let count = if chroma_format_idc == 3 { 12 } else { 8 };
                scaling_matrix = ScalingMatrix::parse(&mut reader, count, None)?;
            }
        }
        let log2_max_frame_num = narrow(reader.ue_max(12, "log2_max_frame_num_minus4")? + 4);
        let poc_type = match reader.ue_max(2, "pic_order_cnt_type")? {
            0 => PocType::Lsb {
                log2_max_lsb: narrow(reader.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4),
            },
            1 => {
                let delta_pic_order_always_zero_flag = reader.flag()?;
                let offset_for_non_ref_pic = reader.se()?;
                let offset_for_top_to_bottom_field = reader.se()?;
                let count = reader.ue_max(255, "num_ref_frames_in_pic_order_cnt_cycle")?;
                let offset_for_ref_frame = (0..count)
                    .map(|_| reader.se())
                    .collect::<Result<Vec<i32>, BitstreamError>>()?;
                PocType::Cycle {
                    delta_pic_order_always_zero_flag,
                    offset_for_non_ref_pic,
                    offset_for_top_to_bottom_field,
                    offset_for_ref_frame,
                }
            }
            _ => PocType::DecodingOrder,
        };
        let max_num_ref_frames = narrow(reader.ue_max(16, "max_num_ref_frames")?);
        let gaps_in_frame_num_value_allowed_flag = reader.flag()?;
        let pic_width_in_mbs = reader.ue_max(1055, "pic_width_in_mbs_minus1")? + 1;
        let pic_height_in_map_units = reader.ue_max(1055, "pic_height_in_map_units_minus1")? + 1;
        let frame_mbs_only_flag = reader.flag()?;
        let mb_adaptive_frame_field_flag = !frame_mbs_only_flag && reader.flag()?;
        let direct_8x8_inference_flag = reader.flag()?;
        let width = pic_width_in_mbs * 16;
        let height = pic_height_in_map_units * 16 * if frame_mbs_only_flag { 1 } else { 2 };
        let crop = if reader.flag()? {
            let chroma_array_type = if separate_colour_plane_flag {
                0
            } else {
                chroma_format_idc
            };
            let (sub_width, sub_height) = match chroma_array_type {
                1 => (2, 2),
                2 => (2, 1),
                _ => (1, 1),
            };
            let unit_x = sub_width;
            let unit_y = sub_height * if frame_mbs_only_flag { 1 } else { 2 };
            let left = reader.ue()?.saturating_mul(unit_x);
            let right = reader.ue()?.saturating_mul(unit_x);
            let top = reader.ue()?.saturating_mul(unit_y);
            let bottom = reader.ue()?.saturating_mul(unit_y);
            if left.saturating_add(right) >= width || top.saturating_add(bottom) >= height {
                return Err(BitstreamError::Invalid("frame_cropping"));
            }
            (left, right, top, bottom)
        } else {
            (0, 0, 0, 0)
        };
        let vui = if reader.flag()? {
            Some(vui_parameters(&mut reader)?)
        } else {
            None
        };
        Ok(Self {
            profile_idc,
            constraint_flags,
            level_idc,
            id,
            chroma_format_idc,
            separate_colour_plane_flag,
            bit_depth_luma,
            bit_depth_chroma,
            qpprime_y_zero_transform_bypass_flag,
            scaling_matrix,
            seq_scaling_matrix_present_flag,
            log2_max_frame_num,
            poc_type,
            max_num_ref_frames,
            gaps_in_frame_num_value_allowed_flag,
            pic_width_in_mbs,
            pic_height_in_map_units,
            frame_mbs_only_flag,
            mb_adaptive_frame_field_flag,
            direct_8x8_inference_flag,
            crop,
            vui,
        })
    }

    /// `FrameHeightInMbs`.
    #[must_use]
    pub const fn frame_height_in_mbs(&self) -> u32 {
        self.pic_height_in_map_units * if self.frame_mbs_only_flag { 1 } else { 2 }
    }

    /// `MaxFrameNum`.
    #[must_use]
    pub const fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
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
}

/// A picture parameter set, resolved against its SPS.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
    /// The scaling matrix in force for pictures using this PPS.
    pub scaling_matrix: ScalingMatrix,
    pub second_chroma_qp_index_offset: i8,
}

impl Pps {
    /// The SPS id a PPS names, read without parsing the rest.
    #[must_use]
    pub fn peek_sps_id(rbsp: &[u8]) -> Option<u8> {
        let mut reader = BitReader::new(rbsp);
        reader.ue().ok()?;
        reader.ue().ok().and_then(|id| u8::try_from(id).ok())
    }

    /// Parses a PPS RBSP (after the one-byte NAL header) against `sps`.
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] on truncation, out-of-range fields, or slice
    /// groups (FMO), which Arcen does not use.
    pub fn parse(rbsp: &[u8], sps: &Sps) -> Result<Self, BitstreamError> {
        let mut reader = BitReader::new(rbsp);
        let id = narrow(reader.ue_max(255, "pic_parameter_set_id")?);
        let sps_id = narrow(reader.ue_max(31, "seq_parameter_set_id")?);
        if sps_id != sps.id {
            return Err(BitstreamError::Invalid("seq_parameter_set_id"));
        }
        let entropy_coding_mode_flag = reader.flag()?;
        let bottom_field_pic_order_in_frame_present_flag = reader.flag()?;
        if reader.ue_max(7, "num_slice_groups_minus1")? > 0 {
            return Err(BitstreamError::Unsupported("H.264 slice groups (FMO)"));
        }
        let num_ref_idx_l0_default_active_minus1 =
            narrow(reader.ue_max(31, "num_ref_idx_l0_default_active_minus1")?);
        let num_ref_idx_l1_default_active_minus1 =
            narrow(reader.ue_max(31, "num_ref_idx_l1_default_active_minus1")?);
        let weighted_pred_flag = reader.flag()?;
        let weighted_bipred_idc = reader.u8(2)?;
        if weighted_bipred_idc > 2 {
            return Err(BitstreamError::Invalid("weighted_bipred_idc"));
        }
        let qp_bd_offset = 6 * (i32::from(sps.bit_depth_luma) - 8);
        let pic_init_qp_minus26 =
            narrow_i8(reader.se_range(-(26 + qp_bd_offset), 25, "pic_init_qp_minus26")?);
        let pic_init_qs_minus26 = narrow_i8(reader.se_range(-26, 25, "pic_init_qs_minus26")?);
        let chroma_qp_index_offset =
            narrow_i8(reader.se_range(-12, 12, "chroma_qp_index_offset")?);
        let deblocking_filter_control_present_flag = reader.flag()?;
        let constrained_intra_pred_flag = reader.flag()?;
        let redundant_pic_cnt_present_flag = reader.flag()?;
        let mut transform_8x8_mode_flag = false;
        let mut scaling_matrix = sps.scaling_matrix.clone();
        let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
        if reader.more_rbsp_data() {
            transform_8x8_mode_flag = reader.flag()?;
            if reader.flag()? {
                let lists_8x8 = if transform_8x8_mode_flag {
                    if sps.chroma_format_idc == 3 { 6 } else { 2 }
                } else {
                    0
                };
                // Rule B falls back to the sequence-level lists, or to the
                // defaults (rule A) when the SPS carries none.
                let base = sps
                    .seq_scaling_matrix_present_flag
                    .then_some(&sps.scaling_matrix);
                scaling_matrix = ScalingMatrix::parse(&mut reader, 6 + lists_8x8, base)?;
            }
            second_chroma_qp_index_offset =
                narrow_i8(reader.se_range(-12, 12, "second_chroma_qp_index_offset")?);
        }
        Ok(Self {
            id,
            sps_id,
            entropy_coding_mode_flag,
            bottom_field_pic_order_in_frame_present_flag,
            num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_default_active_minus1,
            weighted_pred_flag,
            weighted_bipred_idc,
            pic_init_qp_minus26,
            pic_init_qs_minus26,
            chroma_qp_index_offset,
            deblocking_filter_control_present_flag,
            constrained_intra_pred_flag,
            redundant_pic_cnt_present_flag,
            transform_8x8_mode_flag,
            scaling_matrix,
            second_chroma_qp_index_offset,
        })
    }
}

pub(crate) fn narrow(value: u32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

pub(crate) fn narrow_i8(value: i32) -> i8 {
    i8::try_from(value).unwrap_or(if value < 0 { i8::MIN } else { i8::MAX })
}
