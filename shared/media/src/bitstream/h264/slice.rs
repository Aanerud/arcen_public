// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! H.264 slice header (ITU-T H.264 §7.3.3) for frame (progressive) coding.

use super::super::{BitReader, BitstreamError};
use super::params::{PocType, Pps, Sps, narrow};

/// `slice_type % 5`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
}

/// One `ref_pic_list_modification()` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListModification {
    /// `modification_of_pic_nums_idc` 0: subtract `abs_diff_pic_num_minus1 + 1`.
    Subtract(u32),
    /// `modification_of_pic_nums_idc` 1: add `abs_diff_pic_num_minus1 + 1`.
    Add(u32),
    /// `modification_of_pic_nums_idc` 2: a long-term picture.
    LongTerm(u32),
}

/// One `memory_management_control_operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mmco {
    /// 1: a short-term picture becomes unused.
    UnmarkShortTerm { difference_of_pic_nums_minus1: u32 },
    /// 2: a long-term picture becomes unused.
    UnmarkLongTerm { long_term_pic_num: u32 },
    /// 3: a short-term picture becomes long-term.
    ShortToLong {
        difference_of_pic_nums_minus1: u32,
        long_term_frame_idx: u32,
    },
    /// 4: limits long-term frame indices.
    MaxLongTermIndex { max_long_term_frame_idx_plus1: u32 },
    /// 5: every reference becomes unused.
    UnmarkAll,
    /// 6: the current picture becomes long-term.
    CurrentToLong { long_term_frame_idx: u32 },
}

/// `dec_ref_pic_marking()`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefPicMarking {
    pub no_output_of_prior_pics_flag: bool,
    pub long_term_reference_flag: bool,
    /// `Some` with `adaptive_ref_pic_marking_mode_flag`.
    pub adaptive: Option<Vec<Mmco>>,
}

/// The slice header fields picture management and a hardware decoder need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    pub pps_id: u8,
    pub colour_plane_id: u8,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred_flag: bool,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub modification_l0: Option<Vec<ListModification>>,
    pub modification_l1: Option<Vec<ListModification>>,
    pub marking: Option<RefPicMarking>,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
}

impl SliceHeader {
    /// Parses a slice header from a slice NAL unit's RBSP (after the
    /// one-byte header). `nal_ref_idc` and `idr` come from that header.
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] on truncation, out-of-range fields, field coding
    /// or SP/SI slices.
    #[allow(clippy::too_many_lines)]
    pub fn parse(
        rbsp: &[u8],
        nal_ref_idc: u8,
        idr: bool,
        sps: &Sps,
        pps: &Pps,
    ) -> Result<Self, BitstreamError> {
        let mut reader = BitReader::new(rbsp);
        let mbs = sps.pic_width_in_mbs * sps.frame_height_in_mbs();
        let first_mb_in_slice = reader.ue_max(mbs - 1, "first_mb_in_slice")?;
        let slice_type = match reader.ue_max(9, "slice_type")? % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            _ => return Err(BitstreamError::Unsupported("H.264 SP/SI slices")),
        };
        if idr && slice_type != SliceType::I {
            return Err(BitstreamError::Invalid("slice_type of an IDR picture"));
        }
        let pps_id = narrow(reader.ue_max(255, "pic_parameter_set_id")?);
        if pps_id != pps.id {
            return Err(BitstreamError::Invalid("pic_parameter_set_id"));
        }
        let colour_plane_id = if sps.separate_colour_plane_flag {
            reader.u8(2)?
        } else {
            0
        };
        let frame_num = reader.u(u32::from(sps.log2_max_frame_num))?;
        if !sps.frame_mbs_only_flag && reader.flag()? {
            return Err(BitstreamError::Unsupported("H.264 field pictures"));
        }
        if idr && frame_num != 0 {
            return Err(BitstreamError::Invalid("frame_num of an IDR picture"));
        }
        let idr_pic_id = if idr {
            reader.ue_max(65535, "idr_pic_id")?
        } else {
            0
        };
        let mut pic_order_cnt_lsb = 0;
        let mut delta_pic_order_cnt_bottom = 0;
        let mut delta_pic_order_cnt = [0; 2];
        match &sps.poc_type {
            PocType::Lsb { log2_max_lsb } => {
                pic_order_cnt_lsb = reader.u(u32::from(*log2_max_lsb))?;
                if pps.bottom_field_pic_order_in_frame_present_flag {
                    delta_pic_order_cnt_bottom = reader.se()?;
                }
            }
            PocType::Cycle {
                delta_pic_order_always_zero_flag: false,
                ..
            } => {
                delta_pic_order_cnt[0] = reader.se()?;
                if pps.bottom_field_pic_order_in_frame_present_flag {
                    delta_pic_order_cnt[1] = reader.se()?;
                }
            }
            _ => {}
        }
        let redundant_pic_cnt = if pps.redundant_pic_cnt_present_flag {
            reader.ue_max(127, "redundant_pic_cnt")?
        } else {
            0
        };
        let direct_spatial_mv_pred_flag = slice_type == SliceType::B && reader.flag()?;
        let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        let mut num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        if slice_type != SliceType::I && reader.flag()? {
            num_ref_idx_l0_active_minus1 =
                narrow(reader.ue_max(15, "num_ref_idx_l0_active_minus1")?);
            if slice_type == SliceType::B {
                num_ref_idx_l1_active_minus1 =
                    narrow(reader.ue_max(15, "num_ref_idx_l1_active_minus1")?);
            }
        }
        if num_ref_idx_l0_active_minus1 > 15 || num_ref_idx_l1_active_minus1 > 15 {
            return Err(BitstreamError::Invalid("num_ref_idx_active_minus1"));
        }
        let mut modification_l0 = None;
        let mut modification_l1 = None;
        if slice_type != SliceType::I {
            if reader.flag()? {
                modification_l0 = Some(list_modification(
                    &mut reader,
                    num_ref_idx_l0_active_minus1,
                )?);
            }
            if slice_type == SliceType::B && reader.flag()? {
                modification_l1 = Some(list_modification(
                    &mut reader,
                    num_ref_idx_l1_active_minus1,
                )?);
            }
        }
        if (pps.weighted_pred_flag && slice_type == SliceType::P)
            || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B)
        {
            pred_weight_table(
                &mut reader,
                sps,
                slice_type,
                num_ref_idx_l0_active_minus1,
                num_ref_idx_l1_active_minus1,
            )?;
        }
        let marking = if nal_ref_idc != 0 {
            Some(dec_ref_pic_marking(&mut reader, idr)?)
        } else {
            None
        };
        let cabac_init_idc = if pps.entropy_coding_mode_flag && slice_type != SliceType::I {
            narrow(reader.ue_max(2, "cabac_init_idc")?)
        } else {
            0
        };
        let qp_bd_offset = 6 * (i32::from(sps.bit_depth_luma) - 8);
        let slice_qp_delta =
            reader.se_range(-(51 + qp_bd_offset), 51 + qp_bd_offset, "slice_qp_delta")?;
        let mut disable_deblocking_filter_idc = 0;
        let mut slice_alpha_c0_offset_div2 = 0;
        let mut slice_beta_offset_div2 = 0;
        if pps.deblocking_filter_control_present_flag {
            disable_deblocking_filter_idc =
                narrow(reader.ue_max(2, "disable_deblocking_filter_idc")?);
            if disable_deblocking_filter_idc != 1 {
                slice_alpha_c0_offset_div2 =
                    reader.se_range(-6, 6, "slice_alpha_c0_offset_div2")?;
                slice_beta_offset_div2 = reader.se_range(-6, 6, "slice_beta_offset_div2")?;
            }
        }
        Ok(Self {
            first_mb_in_slice,
            slice_type,
            pps_id,
            colour_plane_id,
            frame_num,
            idr_pic_id,
            pic_order_cnt_lsb,
            delta_pic_order_cnt_bottom,
            delta_pic_order_cnt,
            redundant_pic_cnt,
            direct_spatial_mv_pred_flag,
            num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1,
            modification_l0,
            modification_l1,
            marking,
            cabac_init_idc,
            slice_qp_delta,
            disable_deblocking_filter_idc,
            slice_alpha_c0_offset_div2,
            slice_beta_offset_div2,
        })
    }

    /// The PPS id of a slice, read without parsing the rest.
    #[must_use]
    pub fn peek_pps_id(rbsp: &[u8]) -> Option<u8> {
        let mut reader = BitReader::new(rbsp);
        reader.ue().ok()?;
        reader.ue().ok()?;
        reader.ue().ok().and_then(|id| u8::try_from(id).ok())
    }
}

fn list_modification(
    reader: &mut BitReader<'_>,
    active_minus1: u8,
) -> Result<Vec<ListModification>, BitstreamError> {
    let mut operations = Vec::new();
    loop {
        let operation = match reader.ue_max(3, "modification_of_pic_nums_idc")? {
            0 => ListModification::Subtract(reader.ue_max(131_071, "abs_diff_pic_num_minus1")? + 1),
            1 => ListModification::Add(reader.ue_max(131_071, "abs_diff_pic_num_minus1")? + 1),
            2 => ListModification::LongTerm(reader.ue_max(31, "long_term_pic_num")?),
            _ => return Ok(operations),
        };
        if operations.len() > usize::from(active_minus1) {
            return Err(BitstreamError::Invalid("ref_pic_list_modification length"));
        }
        operations.push(operation);
    }
}

fn pred_weight_table(
    reader: &mut BitReader<'_>,
    sps: &Sps,
    slice_type: SliceType,
    l0_minus1: u8,
    l1_minus1: u8,
) -> Result<(), BitstreamError> {
    reader.ue_max(7, "luma_log2_weight_denom")?;
    let chroma = sps.chroma_array_type() != 0;
    if chroma {
        reader.ue_max(7, "chroma_log2_weight_denom")?;
    }
    let mut lists = vec![l0_minus1];
    if slice_type == SliceType::B {
        lists.push(l1_minus1);
    }
    for active_minus1 in lists {
        for _ in 0..=active_minus1 {
            if reader.flag()? {
                reader.se_range(-128, 127, "luma_weight")?;
                reader.se_range(-128, 127, "luma_offset")?;
            }
            if chroma && reader.flag()? {
                for _ in 0..2 {
                    reader.se_range(-128, 127, "chroma_weight")?;
                    reader.se_range(-128, 127, "chroma_offset")?;
                }
            }
        }
    }
    Ok(())
}

fn dec_ref_pic_marking(
    reader: &mut BitReader<'_>,
    idr: bool,
) -> Result<RefPicMarking, BitstreamError> {
    let mut marking = RefPicMarking::default();
    if idr {
        marking.no_output_of_prior_pics_flag = reader.flag()?;
        marking.long_term_reference_flag = reader.flag()?;
        return Ok(marking);
    }
    if !reader.flag()? {
        return Ok(marking);
    }
    let mut operations = Vec::new();
    loop {
        let operation = match reader.ue_max(6, "memory_management_control_operation")? {
            0 => break,
            1 => Mmco::UnmarkShortTerm {
                difference_of_pic_nums_minus1: reader
                    .ue_max(131_071, "difference_of_pic_nums_minus1")?,
            },
            2 => Mmco::UnmarkLongTerm {
                long_term_pic_num: reader.ue_max(31, "long_term_pic_num")?,
            },
            3 => Mmco::ShortToLong {
                difference_of_pic_nums_minus1: reader
                    .ue_max(131_071, "difference_of_pic_nums_minus1")?,
                long_term_frame_idx: reader.ue_max(15, "long_term_frame_idx")?,
            },
            4 => Mmco::MaxLongTermIndex {
                max_long_term_frame_idx_plus1: reader
                    .ue_max(16, "max_long_term_frame_idx_plus1")?,
            },
            5 => Mmco::UnmarkAll,
            _ => Mmco::CurrentToLong {
                long_term_frame_idx: reader.ue_max(15, "long_term_frame_idx")?,
            },
        };
        if operations.len() >= 66 {
            return Err(BitstreamError::Invalid(
                "memory_management_control_operation count",
            ));
        }
        operations.push(operation);
    }
    marking.adaptive = Some(operations);
    Ok(marking)
}
