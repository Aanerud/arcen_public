// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! H.265 slice segment header (ITU-T H.265 §7.3.6).

use super::super::picture::ceil_log2;
use super::super::{BitReader, BitstreamError};
use super::params::{Pps, ShortTermRps, Sps, narrow, narrow_u32};
use super::{NalHeader, nal_type};

/// `slice_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    B,
    P,
    I,
}

/// One long-term reference entry of a slice header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongTermEntry {
    /// `PocLsbLt[i]`.
    pub poc_lsb: u32,
    /// `UsedByCurrPicLt[i]`.
    pub used_by_curr: bool,
    /// `delta_poc_msb_present_flag[i]`.
    pub msb_present: bool,
    /// `DeltaPocMsbCycleLt[i]` (accumulated per §7.4.7.1).
    pub delta_poc_msb_cycle: u32,
}

/// The fields of a slice segment header that picture management and a
/// hardware decoder need.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SliceHeader {
    pub first_slice_segment_in_pic_flag: bool,
    pub no_output_of_prior_pics_flag: bool,
    pub pps_id: u8,
    pub dependent_slice_segment_flag: bool,
    pub slice_segment_address: u32,
    pub slice_type: SliceType,
    pub pic_output_flag: bool,
    pub colour_plane_id: u8,
    pub pic_order_cnt_lsb: u32,
    /// The short-term RPS in force (from the SPS or coded here).
    pub short_term_rps: Option<ShortTermRps>,
    /// Whether the RPS was coded in this header.
    pub short_term_rps_in_header: bool,
    /// Bits the header's own `st_ref_pic_set()` took
    /// (`wNumBitsForShortTermRPSInSlice`).
    pub short_term_rps_bits: u32,
    pub long_term: Vec<LongTermEntry>,
    pub slice_temporal_mvp_enabled_flag: bool,
    pub slice_sao_luma_flag: bool,
    pub slice_sao_chroma_flag: bool,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    /// `list_entry_l0`, when `ref_pic_list_modification_flag_l0`.
    pub list_entry_l0: Option<Vec<u8>>,
    pub list_entry_l1: Option<Vec<u8>>,
    pub mvd_l1_zero_flag: bool,
    pub cabac_init_flag: bool,
    pub collocated_from_l0_flag: bool,
    pub collocated_ref_idx: u8,
    pub five_minus_max_num_merge_cand: u8,
    pub slice_qp_delta: i32,
    pub slice_cb_qp_offset: i32,
    pub slice_cr_qp_offset: i32,
    pub cu_chroma_qp_offset_enabled_flag: bool,
    pub deblocking_filter_disabled_flag: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices_enabled_flag: bool,
    pub num_entry_point_offsets: u32,
}

impl SliceHeader {
    /// `NumPicTotalCurr` (§7.4.7.2) for this header.
    #[must_use]
    pub fn num_pic_total_curr(&self) -> usize {
        self.short_term_rps
            .as_ref()
            .map_or(0, ShortTermRps::num_used)
            + self
                .long_term
                .iter()
                .filter(|entry| entry.used_by_curr)
                .count()
    }

    /// Parses a slice segment header from a slice NAL unit's RBSP (after the
    /// two-byte NAL header). A dependent slice segment copies the fields of
    /// `previous`, the header of the slice it continues.
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] on truncation, out-of-range fields, or a dependent
    /// slice segment with no independent one before it.
    #[allow(clippy::too_many_lines)]
    pub fn parse(
        rbsp: &[u8],
        nal: NalHeader,
        lookup: impl Fn(u8) -> Option<(std::sync::Arc<Pps>, std::sync::Arc<Sps>)>,
        previous: Option<&Self>,
    ) -> Result<(Self, std::sync::Arc<Pps>, std::sync::Arc<Sps>), BitstreamError> {
        let mut reader = BitReader::new(rbsp);
        let first_slice_segment_in_pic_flag = reader.flag()?;
        let no_output_of_prior_pics_flag = (nal_type::BLA_W_LP..=nal_type::RSV_IRAP_23)
            .contains(&nal.unit_type)
            && reader.flag()?;
        let pps_id = narrow(reader.ue_max(63, "slice_pic_parameter_set_id")?);
        let (pps, sps) = lookup(pps_id).ok_or(BitstreamError::MissingParameterSet {
            kind: "PPS",
            id: u32::from(pps_id),
        })?;
        let mut dependent_slice_segment_flag = false;
        let mut slice_segment_address = 0;
        if !first_slice_segment_in_pic_flag {
            if pps.dependent_slice_segments_enabled_flag {
                dependent_slice_segment_flag = reader.flag()?;
            }
            slice_segment_address = reader.u(sps.slice_segment_address_bits())?;
            if slice_segment_address >= sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs() {
                return Err(BitstreamError::Invalid("slice_segment_address"));
            }
        }
        let mut header = if dependent_slice_segment_flag {
            let mut copy = previous
                .ok_or(BitstreamError::Invalid("dependent_slice_segment_flag"))?
                .clone();
            copy.first_slice_segment_in_pic_flag = false;
            copy.dependent_slice_segment_flag = true;
            copy.slice_segment_address = slice_segment_address;
            copy.no_output_of_prior_pics_flag = no_output_of_prior_pics_flag;
            copy
        } else {
            Self::parse_independent(
                &mut reader,
                nal,
                &pps,
                &sps,
                first_slice_segment_in_pic_flag,
                no_output_of_prior_pics_flag,
                pps_id,
                slice_segment_address,
            )?
        };
        header.num_entry_point_offsets = 0;
        if pps.tiles.is_some() || pps.entropy_coding_sync_enabled_flag {
            let ctbs = sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs();
            let offsets = reader.ue_max(ctbs, "num_entry_point_offsets")?;
            if offsets > 0 {
                let bits = reader.ue_max(31, "offset_len_minus1")? + 1;
                for _ in 0..offsets {
                    reader.skip(bits as usize)?;
                }
            }
            header.num_entry_point_offsets = offsets;
        }
        if pps.slice_segment_header_extension_present_flag {
            let length = reader.ue_max(256, "slice_segment_header_extension_length")?;
            reader.skip(8 * length as usize)?;
        }
        // byte_alignment(): a one bit, then zeros to the byte boundary.
        if reader.bit()? != 1 {
            return Err(BitstreamError::Invalid("byte_alignment"));
        }
        while !reader.byte_aligned() {
            if reader.bit()? != 0 {
                return Err(BitstreamError::Invalid("byte_alignment"));
            }
        }
        Ok((header, pps, sps))
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn parse_independent(
        reader: &mut BitReader<'_>,
        nal: NalHeader,
        pps: &Pps,
        sps: &Sps,
        first_slice_segment_in_pic_flag: bool,
        no_output_of_prior_pics_flag: bool,
        pps_id: u8,
        slice_segment_address: u32,
    ) -> Result<Self, BitstreamError> {
        reader.skip(usize::from(pps.num_extra_slice_header_bits))?;
        let slice_type = match reader.ue_max(2, "slice_type")? {
            0 => SliceType::B,
            1 => SliceType::P,
            _ => SliceType::I,
        };
        if nal.is_irap() && slice_type != SliceType::I {
            return Err(BitstreamError::Invalid("slice_type of an IRAP picture"));
        }
        let pic_output_flag = !pps.output_flag_present_flag || reader.flag()?;
        let colour_plane_id = if sps.separate_colour_plane_flag {
            reader.u8(2)?
        } else {
            0
        };
        let mut header = Self {
            first_slice_segment_in_pic_flag,
            no_output_of_prior_pics_flag,
            pps_id,
            dependent_slice_segment_flag: false,
            slice_segment_address,
            slice_type,
            pic_output_flag,
            colour_plane_id,
            pic_order_cnt_lsb: 0,
            short_term_rps: None,
            short_term_rps_in_header: false,
            short_term_rps_bits: 0,
            long_term: Vec::new(),
            slice_temporal_mvp_enabled_flag: false,
            slice_sao_luma_flag: false,
            slice_sao_chroma_flag: false,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            list_entry_l0: None,
            list_entry_l1: None,
            mvd_l1_zero_flag: false,
            cabac_init_flag: false,
            collocated_from_l0_flag: true,
            collocated_ref_idx: 0,
            five_minus_max_num_merge_cand: 0,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            cu_chroma_qp_offset_enabled_flag: false,
            deblocking_filter_disabled_flag: pps.pps_deblocking_filter_disabled_flag,
            beta_offset_div2: i32::from(pps.beta_offset_div2),
            tc_offset_div2: i32::from(pps.tc_offset_div2),
            loop_filter_across_slices_enabled_flag: pps.loop_filter_across_slices_enabled_flag,
            num_entry_point_offsets: 0,
        };
        if !nal.is_idr() {
            header.pic_order_cnt_lsb = reader.u(u32::from(sps.log2_max_pic_order_cnt_lsb))?;
            let buffering = u32::from(sps.highest_ordering().max_dec_pic_buffering_minus1);
            if reader.flag()? {
                // short_term_ref_pic_set_sps_flag
                let count = sps.short_term_ref_pic_sets.len();
                if count == 0 {
                    return Err(BitstreamError::Invalid("short_term_ref_pic_set_sps_flag"));
                }
                let bits = ceil_log2(narrow_u32(count));
                let index = reader.u(bits)? as usize;
                header.short_term_rps = Some(
                    sps.short_term_ref_pic_sets
                        .get(index)
                        .ok_or(BitstreamError::Invalid("short_term_ref_pic_set_idx"))?
                        .clone(),
                );
            } else {
                let start = reader.position();
                header.short_term_rps = Some(ShortTermRps::parse(
                    reader,
                    &sps.short_term_ref_pic_sets,
                    true,
                    buffering,
                )?);
                header.short_term_rps_in_header = true;
                header.short_term_rps_bits = narrow_u32(reader.position() - start);
            }
            if sps.long_term_ref_pics_present_flag {
                header.long_term = parse_long_term(reader, sps)?;
            }
            if sps.sps_temporal_mvp_enabled_flag {
                header.slice_temporal_mvp_enabled_flag = reader.flag()?;
            }
            let total = header
                .short_term_rps
                .as_ref()
                .map_or(0, ShortTermRps::num_delta_pocs)
                + header.long_term.len();
            if total > usize::from(sps.highest_ordering().max_dec_pic_buffering_minus1) + 1 {
                return Err(BitstreamError::Invalid("reference picture set size"));
            }
        }
        if sps.sample_adaptive_offset_enabled_flag {
            header.slice_sao_luma_flag = reader.flag()?;
            if sps.chroma_array_type() != 0 {
                header.slice_sao_chroma_flag = reader.flag()?;
            }
        }
        if slice_type != SliceType::I {
            Self::parse_inter(reader, &mut header, pps, sps)?;
        }
        header.slice_qp_delta = reader.se_range(-87, 77, "slice_qp_delta")?;
        if pps.slice_chroma_qp_offsets_present_flag {
            header.slice_cb_qp_offset = reader.se_range(-12, 12, "slice_cb_qp_offset")?;
            header.slice_cr_qp_offset = reader.se_range(-12, 12, "slice_cr_qp_offset")?;
        }
        if pps.range_extension.chroma_qp_offset_list_enabled_flag {
            header.cu_chroma_qp_offset_enabled_flag = reader.flag()?;
        }
        let override_flag = pps.deblocking_filter_override_enabled_flag && reader.flag()?;
        if override_flag {
            header.deblocking_filter_disabled_flag = reader.flag()?;
            if !header.deblocking_filter_disabled_flag {
                header.beta_offset_div2 = reader.se_range(-6, 6, "slice_beta_offset_div2")?;
                header.tc_offset_div2 = reader.se_range(-6, 6, "slice_tc_offset_div2")?;
            }
        }
        if pps.loop_filter_across_slices_enabled_flag
            && (header.slice_sao_luma_flag
                || header.slice_sao_chroma_flag
                || !header.deblocking_filter_disabled_flag)
        {
            header.loop_filter_across_slices_enabled_flag = reader.flag()?;
        }
        Ok(header)
    }

    fn parse_inter(
        reader: &mut BitReader<'_>,
        header: &mut Self,
        pps: &Pps,
        sps: &Sps,
    ) -> Result<(), BitstreamError> {
        let is_b = header.slice_type == SliceType::B;
        header.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        header.num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        if reader.flag()? {
            header.num_ref_idx_l0_active_minus1 =
                narrow(reader.ue_max(14, "num_ref_idx_l0_active_minus1")?);
            if is_b {
                header.num_ref_idx_l1_active_minus1 =
                    narrow(reader.ue_max(14, "num_ref_idx_l1_active_minus1")?);
            }
        }
        let total = header.num_pic_total_curr();
        if total == 0 {
            return Err(BitstreamError::Invalid("NumPicTotalCurr of an inter slice"));
        }
        if total > 8 {
            // H.265 allows at most 8 (§7.4.7.2); DXVA holds 8 per set.
            return Err(BitstreamError::Invalid("NumPicTotalCurr"));
        }
        if pps.lists_modification_present_flag && total > 1 {
            let bits = ceil_log2(narrow_u32(total));
            if reader.flag()? {
                header.list_entry_l0 = Some(list_entries(
                    reader,
                    header.num_ref_idx_l0_active_minus1,
                    bits,
                    total,
                )?);
            }
            if is_b && reader.flag()? {
                header.list_entry_l1 = Some(list_entries(
                    reader,
                    header.num_ref_idx_l1_active_minus1,
                    bits,
                    total,
                )?);
            }
        }
        if is_b {
            header.mvd_l1_zero_flag = reader.flag()?;
        }
        if pps.cabac_init_present_flag {
            header.cabac_init_flag = reader.flag()?;
        }
        if header.slice_temporal_mvp_enabled_flag {
            if is_b {
                header.collocated_from_l0_flag = reader.flag()?;
            }
            let active = if header.collocated_from_l0_flag {
                header.num_ref_idx_l0_active_minus1
            } else {
                header.num_ref_idx_l1_active_minus1
            };
            if active > 0 {
                header.collocated_ref_idx =
                    narrow(reader.ue_max(u32::from(active), "collocated_ref_idx")?);
            }
        }
        if (pps.weighted_pred_flag && header.slice_type == SliceType::P)
            || (pps.weighted_bipred_flag && is_b)
        {
            pred_weight_table(reader, header, sps)?;
        }
        header.five_minus_max_num_merge_cand =
            narrow(reader.ue_max(4, "five_minus_max_num_merge_cand")?);
        Ok(())
    }
}

fn list_entries(
    reader: &mut BitReader<'_>,
    active_minus1: u8,
    bits: u32,
    total: usize,
) -> Result<Vec<u8>, BitstreamError> {
    (0..=active_minus1)
        .map(|_| {
            let entry = reader.u(bits)?;
            if entry as usize >= total {
                return Err(BitstreamError::Invalid("list_entry"));
            }
            Ok(narrow(entry))
        })
        .collect()
}

fn parse_long_term(
    reader: &mut BitReader<'_>,
    sps: &Sps,
) -> Result<Vec<LongTermEntry>, BitstreamError> {
    let candidates = narrow_u32(sps.long_term_ref_pics_sps.len());
    let num_long_term_sps = if candidates > 0 {
        reader.ue_max(candidates, "num_long_term_sps")?
    } else {
        0
    };
    let num_long_term_pics = reader.ue_max(
        super::params::MAX_DPB_SIZE - num_long_term_sps.min(super::params::MAX_DPB_SIZE),
        "num_long_term_pics",
    )?;
    let mut entries: Vec<LongTermEntry> = Vec::new();
    for index in 0..num_long_term_sps + num_long_term_pics {
        let (poc_lsb, used_by_curr) = if index < num_long_term_sps {
            let candidate = if candidates > 1 {
                reader.u(ceil_log2(candidates))? as usize
            } else {
                0
            };
            *sps.long_term_ref_pics_sps
                .get(candidate)
                .ok_or(BitstreamError::Invalid("lt_idx_sps"))?
        } else {
            (
                reader.u(u32::from(sps.log2_max_pic_order_cnt_lsb))?,
                reader.flag()?,
            )
        };
        let msb_present = reader.flag()?;
        let mut delta_poc_msb_cycle = if msb_present {
            reader.ue_max(1 << 24, "delta_poc_msb_cycle_lt")?
        } else {
            0
        };
        // (7-52): the cycle accumulates within each of the two groups.
        if index != 0 && index != num_long_term_sps {
            if let Some(previous) = entries.last() {
                delta_poc_msb_cycle =
                    delta_poc_msb_cycle.saturating_add(previous.delta_poc_msb_cycle);
            }
        }
        entries.push(LongTermEntry {
            poc_lsb,
            used_by_curr,
            msb_present,
            delta_poc_msb_cycle,
        });
    }
    Ok(entries)
}

/// `pred_weight_table()` (§7.3.6.3): read and bounds-checked, not kept (a
/// DXVA decoder parses it from the slice data itself).
fn pred_weight_table(
    reader: &mut BitReader<'_>,
    header: &SliceHeader,
    sps: &Sps,
) -> Result<(), BitstreamError> {
    let chroma = sps.chroma_array_type() != 0;
    let wp_offset: i32 = if sps.range_extension.high_precision_offsets_enabled_flag {
        1_i32 << (u32::from(sps.bit_depth_luma.max(sps.bit_depth_chroma)) - 1)
    } else {
        128
    };
    reader.ue_max(7, "luma_log2_weight_denom")?;
    if chroma {
        reader.se_range(-7, 7, "delta_chroma_log2_weight_denom")?;
    }
    let lists = if header.slice_type == SliceType::B {
        vec![
            header.num_ref_idx_l0_active_minus1,
            header.num_ref_idx_l1_active_minus1,
        ]
    } else {
        vec![header.num_ref_idx_l0_active_minus1]
    };
    for active_minus1 in lists {
        let count = usize::from(active_minus1) + 1;
        let mut luma = vec![false; count];
        let mut chroma_flags = vec![false; count];
        for flag in &mut luma {
            *flag = reader.flag()?;
        }
        if chroma {
            for flag in &mut chroma_flags {
                *flag = reader.flag()?;
            }
        }
        for index in 0..count {
            if luma[index] {
                reader.se_range(-128, 127, "delta_luma_weight")?;
                reader.se_range(-wp_offset, wp_offset - 1, "luma_offset")?;
            }
            if chroma_flags[index] {
                for _ in 0..2 {
                    reader.se_range(-128, 127, "delta_chroma_weight")?;
                    reader.se_range(-4 * wp_offset, 4 * wp_offset - 1, "delta_chroma_offset")?;
                }
            }
        }
    }
    Ok(())
}
