// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! DXVA buffer contents for H.264 and HEVC (including the format range
//! extensions), filled from `arcen-media`'s decoder-independent picture
//! models.
//!
//! The structures are the documented `dxva.h` layouts (byte-packed, as the
//! header declares them); `windows` 0.58 does not carry them. Field order,
//! bit positions and sizes follow the Windows driver documentation
//! (`DXVA_PicParams_HEVC`, `DXVA_PicParams_HEVC_RangeExt`,
//! `DXVA_PicParams_H264`, `DXVA_Qmatrix_*`, `DXVA_Slice_*_Short`), and the
//! sizes are asserted at compile time.

use arcen_media::bitstream::h264::H264Picture;
use arcen_media::bitstream::hevc::HevcPicture;

/// `DXVA_PicEntry_*` value for "no picture".
pub const NO_PICTURE: u8 = 0xff;

/// `DXVA_PicParams_HEVC`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
#[allow(non_snake_case, missing_debug_implementations)]
pub struct PicParamsHevc {
    pub PicWidthInMinCbsY: u16,
    pub PicHeightInMinCbsY: u16,
    pub wFormatAndSequenceInfoFlags: u16,
    pub CurrPic: u8,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub ucNumDeltaPocsOfRefRpsIdx: u8,
    pub wNumBitsForShortTermRPSInSlice: u16,
    pub ReservedBits2: u16,
    pub dwCodingParamToolFlags: u32,
    pub dwCodingSettingPicturePropertyFlags: u32,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub diff_cu_qp_delta_depth: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub CurrPicOrderCntVal: i32,
    pub RefPicList: [u8; 15],
    pub ReservedBits5: u8,
    pub PicOrderCntValList: [i32; 15],
    pub RefPicSetStCurrBefore: [u8; 8],
    pub RefPicSetStCurrAfter: [u8; 8],
    pub RefPicSetLtCurr: [u8; 8],
    pub ReservedBits6: u16,
    pub ReservedBits7: u16,
    pub StatusReportFeedbackNumber: u32,
}

/// `DXVA_PicParams_HEVC_RangeExt` (Windows 11 24H2 SDK): the base
/// parameters followed by the range extension fields.
#[repr(C, packed)]
#[derive(Clone, Copy)]
#[allow(non_snake_case, missing_debug_implementations)]
pub struct PicParamsHevcRangeExt {
    pub params: PicParamsHevc,
    pub dwRangeExtensionFlags: u16,
    pub diff_cu_chroma_qp_offset_depth: u8,
    pub log2_sao_offset_scale_luma: u8,
    pub log2_sao_offset_scale_chroma: u8,
    pub log2_max_transform_skip_block_size_minus2: u8,
    pub cb_qp_offset_list: [i8; 6],
    pub cr_qp_offset_list: [i8; 6],
    pub chroma_qp_offset_list_len_minus1: u8,
    pub ReservedBits9: u16,
}

/// `DXVA_Qmatrix_HEVC`: lists in coded (up-right diagonal) order.
#[repr(C, packed)]
#[derive(Clone, Copy)]
#[allow(non_snake_case, missing_debug_implementations)]
pub struct QmatrixHevc {
    pub ucScalingLists0: [[u8; 16]; 6],
    pub ucScalingLists1: [[u8; 64]; 6],
    pub ucScalingLists2: [[u8; 64]; 6],
    pub ucScalingLists3: [[u8; 64]; 2],
    pub ucScalingListDCCoefSizeID2: [u8; 6],
    pub ucScalingListDCCoefSizeID3: [u8; 2],
}

/// `DXVA_Slice_HEVC_Short` and `DXVA_Slice_H264_Short`.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
#[allow(non_snake_case)]
pub struct SliceShort {
    pub BSNALunitDataLocation: u32,
    pub SliceBytesInBuffer: u32,
    pub wBadSliceChopping: u16,
}

impl std::fmt::Debug for SliceShort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (location, bytes) = (self.BSNALunitDataLocation, self.SliceBytesInBuffer);
        formatter
            .debug_struct("SliceShort")
            .field("BSNALunitDataLocation", &location)
            .field("SliceBytesInBuffer", &bytes)
            .finish_non_exhaustive()
    }
}

/// `DXVA_PicParams_H264`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
#[allow(non_snake_case, missing_debug_implementations)]
pub struct PicParamsH264 {
    pub wFrameWidthInMbsMinus1: u16,
    pub wFrameHeightInMbsMinus1: u16,
    pub CurrPic: u8,
    pub num_ref_frames: u8,
    pub wBitFields: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub Reserved16Bits: u16,
    pub StatusReportFeedbackNumber: u32,
    pub RefFrameList: [u8; 16],
    pub CurrFieldOrderCnt: [i32; 2],
    pub FieldOrderCntList: [[i32; 2]; 16],
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub ContinuationFlag: u8,
    pub pic_init_qp_minus26: i8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub Reserved8BitsA: u8,
    pub FrameNumList: [u16; 16],
    pub UsedForReferenceFlags: u32,
    pub NonExistingFrameFlags: u16,
    pub frame_num: u16,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero_flag: u8,
    pub direct_8x8_inference_flag: u8,
    pub entropy_coding_mode_flag: u8,
    pub pic_order_present_flag: u8,
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub deblocking_filter_control_present_flag: u8,
    pub redundant_pic_cnt_present_flag: u8,
    pub Reserved8BitsB: u8,
    pub slice_group_change_rate_minus1: u16,
    pub SliceGroupMap: [u8; 810],
}

/// `DXVA_Qmatrix_H264`: lists in coded (zig-zag) order.
#[repr(C, packed)]
#[derive(Clone, Copy)]
#[allow(non_snake_case, missing_debug_implementations)]
pub struct QmatrixH264 {
    pub bScalingLists4x4: [[u8; 16]; 6],
    pub bScalingLists8x8: [[u8; 64]; 2],
}

const _: () = assert!(size_of::<PicParamsHevc>() == 232);
const _: () = assert!(size_of::<PicParamsHevcRangeExt>() == 253);
const _: () = assert!(size_of::<QmatrixHevc>() == 1000);
const _: () = assert!(size_of::<SliceShort>() == 10);
const _: () = assert!(size_of::<PicParamsH264>() == 1040);
const _: () = assert!(size_of::<QmatrixH264>() == 224);

/// The bytes of a plain-old-data DXVA structure.
#[must_use]
pub fn bytes_of<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: every type passed here is a `repr(C, packed)` struct of
    // integers and integer arrays: no padding, no invalid byte patterns.
    #[allow(unsafe_code)]
    unsafe {
        std::slice::from_raw_parts(std::ptr::from_ref(value).cast::<u8>(), size_of::<T>())
    }
}

fn flag(value: bool, bit: u32) -> u32 {
    u32::from(value) << bit
}

fn slot_entry(slot: u8, long_term: bool) -> u8 {
    (slot & 0x7f) | if long_term { 0x80 } else { 0 }
}

/// Fills `DXVA_PicParams_HEVC` (and, for range-extension streams, the
/// `RangeExt` tail) for `picture`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn hevc_picture_parameters(picture: &HevcPicture, status_report: u32) -> PicParamsHevcRangeExt {
    let sps = &picture.sps;
    let pps = &picture.pps;
    let ordering = sps.highest_ordering();
    let pcm = sps.pcm.unwrap_or_default();
    let tiles = pps.tiles.clone().unwrap_or_default();
    let format_flags = u32::from(sps.chroma_format_idc)
        | flag(sps.separate_colour_plane_flag, 2)
        | (u32::from(sps.bit_depth_luma - 8) << 3)
        | (u32::from(sps.bit_depth_chroma - 8) << 6)
        | (u32::from(sps.log2_max_pic_order_cnt_lsb - 4) << 9)
        | flag(ordering.max_num_reorder_pics == 0, 13);
    let tool_flags = flag(sps.scaling_list_enabled_flag, 0)
        | flag(sps.amp_enabled_flag, 1)
        | flag(sps.sample_adaptive_offset_enabled_flag, 2)
        | flag(sps.pcm.is_some(), 3)
        | (u32::from(pcm.sample_bit_depth_luma_minus1 & 0xf) << 4)
        | (u32::from(pcm.sample_bit_depth_chroma_minus1 & 0xf) << 8)
        | (u32::from(pcm.log2_min_pcm_luma_coding_block_size_minus3 & 3) << 12)
        | (u32::from(pcm.log2_diff_max_min_pcm_luma_coding_block_size & 3) << 14)
        | flag(pcm.loop_filter_disabled_flag, 16)
        | flag(sps.long_term_ref_pics_present_flag, 17)
        | flag(sps.sps_temporal_mvp_enabled_flag, 18)
        | flag(sps.strong_intra_smoothing_enabled_flag, 19)
        | flag(pps.dependent_slice_segments_enabled_flag, 20)
        | flag(pps.output_flag_present_flag, 21)
        | (u32::from(pps.num_extra_slice_header_bits & 7) << 22)
        | flag(pps.sign_data_hiding_enabled_flag, 25)
        | flag(pps.cabac_init_present_flag, 26);
    let property_flags = flag(pps.constrained_intra_pred_flag, 0)
        | flag(pps.transform_skip_enabled_flag, 1)
        | flag(pps.cu_qp_delta_enabled_flag, 2)
        | flag(pps.slice_chroma_qp_offsets_present_flag, 3)
        | flag(pps.weighted_pred_flag, 4)
        | flag(pps.weighted_bipred_flag, 5)
        | flag(pps.transquant_bypass_enabled_flag, 6)
        | flag(pps.tiles.is_some(), 7)
        | flag(pps.entropy_coding_sync_enabled_flag, 8)
        | flag(tiles.uniform_spacing_flag, 9)
        | flag(tiles.loop_filter_across_tiles_enabled_flag, 10)
        | flag(pps.loop_filter_across_slices_enabled_flag, 11)
        | flag(pps.deblocking_filter_override_enabled_flag, 12)
        | flag(pps.pps_deblocking_filter_disabled_flag, 13)
        | flag(pps.lists_modification_present_flag, 14)
        | flag(pps.slice_segment_header_extension_present_flag, 15)
        | flag(picture.irap, 16)
        | flag(picture.idr, 17)
        | flag(picture.intra_only, 18);
    let mut column_width_minus1 = [0_u16; 19];
    for (target, width) in column_width_minus1
        .iter_mut()
        .zip(&picture.tile_columns_minus1)
    {
        *target = *width;
    }
    let mut row_height_minus1 = [0_u16; 21];
    for (target, height) in row_height_minus1.iter_mut().zip(&picture.tile_rows_minus1) {
        *target = *height;
    }
    let mut ref_pic_list = [NO_PICTURE; 15];
    let mut poc_list = [0_i32; 15];
    for (index, reference) in picture.references.iter().take(15).enumerate() {
        ref_pic_list[index] = slot_entry(reference.slot.0, reference.long_term);
        poc_list[index] = reference.poc;
    }
    let set = |indices: &[u8]| {
        let mut out = [NO_PICTURE; 8];
        for (target, index) in out.iter_mut().zip(indices) {
            *target = *index;
        }
        out
    };
    let params = PicParamsHevc {
        PicWidthInMinCbsY: u16::try_from(sps.pic_width_in_min_cbs()).unwrap_or(u16::MAX),
        PicHeightInMinCbsY: u16::try_from(sps.pic_height_in_min_cbs()).unwrap_or(u16::MAX),
        wFormatAndSequenceInfoFlags: u16::try_from(format_flags).unwrap_or(0),
        CurrPic: slot_entry(picture.slot.0, false),
        sps_max_dec_pic_buffering_minus1: ordering.max_dec_pic_buffering_minus1,
        log2_min_luma_coding_block_size_minus3: sps.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: sps.log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2: sps.log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size: sps.log2_diff_max_min_luma_transform_block_size,
        max_transform_hierarchy_depth_inter: sps.max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra: sps.max_transform_hierarchy_depth_intra,
        num_short_term_ref_pic_sets: u8::try_from(sps.short_term_ref_pic_sets.len()).unwrap_or(64),
        num_long_term_ref_pics_sps: u8::try_from(sps.long_term_ref_pics_sps.len()).unwrap_or(32),
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        init_qp_minus26: pps.init_qp_minus26,
        ucNumDeltaPocsOfRefRpsIdx: picture.num_delta_pocs_of_ref_rps_idx,
        wNumBitsForShortTermRPSInSlice: u16::try_from(picture.short_term_rps_bits).unwrap_or(0),
        ReservedBits2: 0,
        dwCodingParamToolFlags: tool_flags,
        dwCodingSettingPicturePropertyFlags: property_flags,
        pps_cb_qp_offset: pps.cb_qp_offset,
        pps_cr_qp_offset: pps.cr_qp_offset,
        num_tile_columns_minus1: tiles.num_tile_columns_minus1,
        num_tile_rows_minus1: tiles.num_tile_rows_minus1,
        column_width_minus1,
        row_height_minus1,
        diff_cu_qp_delta_depth: pps.diff_cu_qp_delta_depth,
        pps_beta_offset_div2: pps.beta_offset_div2,
        pps_tc_offset_div2: pps.tc_offset_div2,
        log2_parallel_merge_level_minus2: pps.log2_parallel_merge_level_minus2,
        CurrPicOrderCntVal: picture.poc,
        RefPicList: ref_pic_list,
        ReservedBits5: 0,
        PicOrderCntValList: poc_list,
        RefPicSetStCurrBefore: set(&picture.st_curr_before),
        RefPicSetStCurrAfter: set(&picture.st_curr_after),
        RefPicSetLtCurr: set(&picture.lt_curr),
        ReservedBits6: 0,
        ReservedBits7: 0,
        StatusReportFeedbackNumber: status_report,
    };
    let range = sps.range_extension;
    let pps_range = &pps.range_extension;
    let range_flags = flag(range.transform_skip_rotation_enabled_flag, 0)
        | flag(range.transform_skip_context_enabled_flag, 1)
        | flag(range.implicit_rdpcm_enabled_flag, 2)
        | flag(range.explicit_rdpcm_enabled_flag, 3)
        | flag(range.extended_precision_processing_flag, 4)
        | flag(range.intra_smoothing_disabled_flag, 5)
        | flag(range.persistent_rice_adaptation_enabled_flag, 6)
        | flag(range.high_precision_offsets_enabled_flag, 7)
        | flag(range.cabac_bypass_alignment_enabled_flag, 8)
        | flag(pps_range.cross_component_prediction_enabled_flag, 9)
        | flag(pps_range.chroma_qp_offset_list_enabled_flag, 10);
    PicParamsHevcRangeExt {
        params,
        dwRangeExtensionFlags: u16::try_from(range_flags).unwrap_or(0),
        diff_cu_chroma_qp_offset_depth: pps_range.diff_cu_chroma_qp_offset_depth,
        log2_sao_offset_scale_luma: pps_range.log2_sao_offset_scale_luma,
        log2_sao_offset_scale_chroma: pps_range.log2_sao_offset_scale_chroma,
        log2_max_transform_skip_block_size_minus2: pps_range
            .log2_max_transform_skip_block_size_minus2,
        cb_qp_offset_list: pps_range.cb_qp_offset_list,
        cr_qp_offset_list: pps_range.cr_qp_offset_list,
        chroma_qp_offset_list_len_minus1: pps_range.chroma_qp_offset_list_len_minus1,
        ReservedBits9: 0,
    }
}

/// `DXVA_Qmatrix_HEVC` for a picture whose stream enables scaling lists.
#[must_use]
pub fn hevc_qmatrix(picture: &HevcPicture) -> Option<QmatrixHevc> {
    let lists = picture.scaling_list.as_ref()?;
    Some(QmatrixHevc {
        ucScalingLists0: lists.size4,
        ucScalingLists1: lists.size8,
        ucScalingLists2: lists.size16,
        ucScalingLists3: [lists.size32[0], lists.size32[3]],
        ucScalingListDCCoefSizeID2: lists.dc16,
        ucScalingListDCCoefSizeID3: [lists.dc32[0], lists.dc32[3]],
    })
}

/// Fills `DXVA_PicParams_H264` for `picture` (frame coding).
#[must_use]
pub fn h264_picture_parameters(picture: &H264Picture, status_report: u32) -> PicParamsH264 {
    let sps = &picture.sps;
    let pps = &picture.pps;
    let bit_fields = (u32::from(sps.chroma_format_idc & 3) << 4)
        | flag(picture.reference, 6)
        | flag(pps.constrained_intra_pred_flag, 7)
        | flag(pps.weighted_pred_flag, 8)
        | (u32::from(pps.weighted_bipred_idc & 3) << 9)
        | flag(true, 11) // MbsConsecutiveFlag: no slice groups
        | flag(sps.frame_mbs_only_flag, 12)
        | flag(pps.transform_8x8_mode_flag, 13)
        | flag(sps.level_idc >= 31, 14)
        | flag(picture.intra_only, 15);
    let mut ref_frame_list = [NO_PICTURE; 16];
    let mut field_order = [[0_i32; 2]; 16];
    let mut frame_numbers = [0_u16; 16];
    let mut used_for_reference = 0_u32;
    for (index, reference) in picture.references.iter().take(16).enumerate() {
        ref_frame_list[index] = slot_entry(reference.slot.0, reference.long_term);
        field_order[index] = [reference.top_poc, reference.bottom_poc];
        frame_numbers[index] = u16::try_from(reference.frame_num_or_long_term_idx).unwrap_or(0);
        used_for_reference |= 3 << (2 * index);
    }
    let (poc_type, lsb_minus4, always_zero) = match &sps.poc_type {
        arcen_media::bitstream::h264::PocType::Lsb { log2_max_lsb } => (0, log2_max_lsb - 4, 0),
        arcen_media::bitstream::h264::PocType::Cycle {
            delta_pic_order_always_zero_flag,
            ..
        } => (1, 0, u8::from(*delta_pic_order_always_zero_flag)),
        arcen_media::bitstream::h264::PocType::DecodingOrder => (2, 0, 0),
    };
    PicParamsH264 {
        wFrameWidthInMbsMinus1: u16::try_from(sps.pic_width_in_mbs - 1).unwrap_or(0),
        wFrameHeightInMbsMinus1: u16::try_from(sps.frame_height_in_mbs() - 1).unwrap_or(0),
        CurrPic: slot_entry(picture.slot.0, false),
        num_ref_frames: sps.max_num_ref_frames,
        wBitFields: u16::try_from(bit_fields).unwrap_or(0),
        bit_depth_luma_minus8: sps.bit_depth_luma - 8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma - 8,
        // The DXVA H.264 specification's "reserved" value every client
        // sends; some drivers decode nothing with 0.
        Reserved16Bits: 3,
        StatusReportFeedbackNumber: status_report,
        RefFrameList: ref_frame_list,
        CurrFieldOrderCnt: [picture.top_poc, picture.bottom_poc],
        FieldOrderCntList: field_order,
        pic_init_qs_minus26: pps.pic_init_qs_minus26,
        chroma_qp_index_offset: pps.chroma_qp_index_offset,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
        ContinuationFlag: 1,
        pic_init_qp_minus26: pps.pic_init_qp_minus26,
        num_ref_idx_l0_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        Reserved8BitsA: 0,
        FrameNumList: frame_numbers,
        UsedForReferenceFlags: used_for_reference,
        NonExistingFrameFlags: 0,
        frame_num: u16::try_from(picture.frame_num).unwrap_or(0),
        log2_max_frame_num_minus4: sps.log2_max_frame_num - 4,
        pic_order_cnt_type: poc_type,
        log2_max_pic_order_cnt_lsb_minus4: lsb_minus4,
        delta_pic_order_always_zero_flag: always_zero,
        direct_8x8_inference_flag: u8::from(sps.direct_8x8_inference_flag),
        entropy_coding_mode_flag: u8::from(pps.entropy_coding_mode_flag),
        pic_order_present_flag: u8::from(pps.bottom_field_pic_order_in_frame_present_flag),
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        deblocking_filter_control_present_flag: u8::from(
            pps.deblocking_filter_control_present_flag,
        ),
        redundant_pic_cnt_present_flag: u8::from(pps.redundant_pic_cnt_present_flag),
        Reserved8BitsB: 0,
        slice_group_change_rate_minus1: 0,
        SliceGroupMap: [0; 810],
    }
}

/// `DXVA_Qmatrix_H264` (always sent; flat when the stream has none).
#[must_use]
pub fn h264_qmatrix(picture: &H264Picture) -> QmatrixH264 {
    let matrix = &picture.pps.scaling_matrix;
    QmatrixH264 {
        bScalingLists4x4: matrix.lists4x4,
        bScalingLists8x8: [matrix.lists8x8[0], matrix.lists8x8[1]],
    }
}

/// The bitstream buffer and its short slice descriptors: each slice as
/// `00 00 01` + NAL unit, the whole padded with zeros to 128 bytes (the
/// padding counted in the last slice).
#[derive(Debug, Default)]
pub struct Bitstream {
    pub data: Vec<u8>,
    pub slices: Vec<SliceShort>,
}

impl Bitstream {
    /// Assembles the slices `nals` (each without a start code).
    #[must_use]
    pub fn assemble<'a>(nals: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut bitstream = Self::default();
        for nal in nals {
            let start = u32::try_from(bitstream.data.len()).unwrap_or(u32::MAX);
            bitstream.data.extend_from_slice(&[0, 0, 1]);
            bitstream.data.extend_from_slice(nal);
            bitstream.slices.push(SliceShort {
                BSNALunitDataLocation: start,
                SliceBytesInBuffer: u32::try_from(nal.len() + 3).unwrap_or(u32::MAX),
                wBadSliceChopping: 0,
            });
        }
        let padding = (128 - bitstream.data.len() % 128) % 128;
        bitstream.data.resize(bitstream.data.len() + padding, 0);
        if let Some(last) = bitstream.slices.last_mut() {
            let size = last.SliceBytesInBuffer;
            last.SliceBytesInBuffer = size + u32::try_from(padding).unwrap_or(0);
        }
        bitstream
    }

    /// The slice descriptors as bytes.
    #[must_use]
    pub fn slice_bytes(&self) -> Vec<u8> {
        self.slices
            .iter()
            .flat_map(|slice| bytes_of(slice).to_vec())
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use arcen_media::annexb::{NalCodec, split_access_units};
    use arcen_media::bitstream::h264::H264Parser;
    use arcen_media::bitstream::hevc::HevcParser;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../shared/media/tests/data/bitstream")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn h264_parameters_describe_the_frame_and_its_reference() {
        let stream = fixture("nvenc-h264-high-640x360.264");
        let units = split_access_units(NalCodec::H264, &stream);
        let mut parser = H264Parser::new(4);
        parser.decode_access_unit(units[0], &|_| false).unwrap();
        let unit = parser.decode_access_unit(units[1], &|_| false).unwrap();
        let picture = unit.picture.unwrap();
        let reference = picture.references[0];
        let parameters = h264_picture_parameters(&picture, 7);
        let (width, height, current, reserved, used, status) = (
            parameters.wFrameWidthInMbsMinus1,
            parameters.wFrameHeightInMbsMinus1,
            parameters.CurrPic,
            parameters.Reserved16Bits,
            parameters.UsedForReferenceFlags,
            parameters.StatusReportFeedbackNumber,
        );
        assert_eq!((width, height), (39, 22));
        assert_eq!(current, picture.slot.0);
        assert_eq!((reserved, status), (3, 7));
        assert_eq!(parameters.RefFrameList[0], reference.slot.0);
        assert_eq!(parameters.RefFrameList[1], NO_PICTURE);
        assert_eq!(used, 3);
        let bits = parameters.wBitFields;
        assert_eq!((bits >> 4) & 3, 1, "chroma_format_idc");
        assert_eq!((bits >> 6) & 1, 1, "RefPicFlag");
        assert_eq!(bits >> 15, 0, "a P picture is not intra");
    }

    #[test]
    fn hevc_range_extension_parameters_carry_444_10_and_the_rps() {
        let stream = fixture("nvenc-hevc-rext444-10-640x360.265");
        let units = split_access_units(NalCodec::H265, &stream);
        let mut parser = HevcParser::new(4);
        let first = parser.decode_access_unit(units[0], &|_| false).unwrap();
        let intra = hevc_picture_parameters(first.picture.as_ref().unwrap(), 1);
        let flags = intra.params.dwCodingSettingPicturePropertyFlags;
        assert_eq!((flags >> 16) & 7, 0b111, "IRAP, IDR and intra");
        let unit = parser.decode_access_unit(units[1], &|_| false).unwrap();
        let picture = unit.picture.unwrap();
        let parameters = hevc_picture_parameters(&picture, 2);
        let format = parameters.params.wFormatAndSequenceInfoFlags;
        assert_eq!(format & 3, 3, "chroma_format_idc");
        assert_eq!((format >> 3) & 7, 2, "bit_depth_luma_minus8");
        assert_eq!((format >> 6) & 7, 2, "bit_depth_chroma_minus8");
        let (before, list, poc) = (
            parameters.params.RefPicSetStCurrBefore,
            parameters.params.RefPicList,
            parameters.params.CurrPicOrderCntVal,
        );
        assert_eq!(before[0], 0);
        assert!(before[1..].iter().all(|entry| *entry == NO_PICTURE));
        assert_eq!(list[0], picture.references[0].slot.0);
        assert_eq!(poc, picture.poc);
        assert_eq!(
            parameters.params.dwCodingSettingPicturePropertyFlags >> 16 & 7,
            0,
            "a trailing picture"
        );
    }

    #[test]
    fn the_bitstream_buffer_is_padded_and_described_slice_by_slice() {
        let first: &[u8] = &[0x26, 0x01, 0xaf, 0x00, 0x00, 0x03, 0x01];
        let second: &[u8] = &[0x02, 0x01, 0xd0];
        let bitstream = Bitstream::assemble([first, second]);
        assert_eq!(bitstream.data.len(), 128);
        assert_eq!(&bitstream.data[..3], &[0, 0, 1]);
        assert_eq!(&bitstream.data[10..13], &[0, 0, 1]);
        let (location, size) = (
            bitstream.slices[1].BSNALunitDataLocation,
            bitstream.slices[1].SliceBytesInBuffer,
        );
        assert_eq!(location, 10);
        assert_eq!(size, 128 - 10);
        assert_eq!(bitstream.slice_bytes().len(), 20);
    }
}
