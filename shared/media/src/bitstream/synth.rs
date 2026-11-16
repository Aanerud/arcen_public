// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Minimal, from-scratch Annex-B parameter sets (VPS/SPS/PPS) for runtime
//! decode-capability probes.
//!
//! Every syntax element is written by an explicit bit writer against the
//! exact ITU-T H.264/H.265 syntax tables rather than copied from a byte
//! literal: a byte blob cannot be reviewed field by field, and a probe's whole
//! purpose is to be an honest, inspectable substitute for parameter sets a
//! Deck has no local encoder to produce.

use super::writer::{BitWriter, escape_emulation_prevention};

/// One synthetic HEVC stream variant a Deck's runtime decode-capability
/// probe builds a real decoder session for (`VideoToolbox` on macOS,
/// `ID3D11VideoDecoder` on Windows). Chroma is restricted to 4:2:0 or 4:4:4 --
/// the only two Arcen's own wire protocol needs this probe to
/// distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HevcProbeProfile {
    /// `chroma_format_idc`: `1` for 4:2:0, `3` for 4:4:4.
    pub chroma_format_idc: u8,
    /// Luma and chroma bit depth (this probe never separates them): `8`,
    /// `10`, or `12`.
    pub bit_depth: u8,
}

impl HevcProbeProfile {
    #[must_use]
    pub const fn ten_bit(self) -> bool {
        self.bit_depth >= 10
    }
}

/// The synthetic picture size every probe builds. One CTU
/// (`log2_diff_max_min_luma_coding_block_size` in [`build_hevc_sps`] picks
/// a 64-sample CTU) at exactly the picture size avoids any
/// boundary/partial-CTU edge case a still-tinier picture would introduce.
pub const PROBE_DIMENSION: u16 = 64;

/// HEVC level to declare in every probed `profile_tier_level` -- Level
/// 1.0, comfortably above what a single `PROBE_DIMENSION` x
/// `PROBE_DIMENSION` picture needs.
pub const PROBE_HEVC_LEVEL_IDC: u8 = 30;

/// `general_profile_idc` a real encoder would choose for `profile`,
/// matching x265/NVENC convention: Main (1) for 8-bit 4:2:0, Main 10 (2)
/// for 10-bit 4:2:0, and Format Range Extensions ("Rext", 4) for
/// everything else this probe exercises (4:4:4 at any depth, or 12-bit
/// 4:2:0 -- "Main 12"). This is the exact axis the whole probe cares
/// about: whether `VideoToolbox` accepts profile 4 (Rext) at all is the
/// only way to learn whether this Mac decodes HEVC Rext, since
/// `chroma_format_idc`/`bit_depth_luma_minus8` alone would still parse
/// even against hardware that only implements a plain Main/Main10
/// decoder.
#[must_use]
pub fn hevc_general_profile_idc(profile: HevcProbeProfile) -> u8 {
    match (profile.chroma_format_idc, profile.bit_depth) {
        (1, 8) => 1,
        (1, 10) => 2,
        _ => 4,
    }
}

/// Writes `profile_tier_level(profilePresentFlag=1, maxNumSubLayersMinus1=0)`
/// (ITU-T H.265 7.3.3): a fixed 96 bits (12 bytes) for the single-layer,
/// no-sub-layers case every VPS/SPS this probe builds uses -- 8
/// (space/tier/idc) + 32 (compatibility flags) + 4 (source/constraint
/// flags) + 43 (profile-dependent constraint/reserved bits, the same
/// total on every branch) + 1 (`inbld`/reserved) + 8 (`level_idc`). Bit
/// widths cross-checked against `FFmpeg`'s `decode_profile_tier_level`
/// (`libavcodec/hevc/ps.c`), independent of and predating this probe.
pub fn write_profile_tier_level(w: &mut BitWriter, profile: HevcProbeProfile) {
    let general_profile_idc = hevc_general_profile_idc(profile);
    w.write_bits(0, 2); // general_profile_space
    w.write_bit(false); // general_tier_flag (Main tier)
    w.write_bits(u32::from(general_profile_idc), 5);
    for compat_idc in 0..32u32 {
        // A profile is always compatible with itself; no other profile's
        // claim applies to a stream synthesised purely to probe one.
        w.write_bit(compat_idc == u32::from(general_profile_idc));
    }
    w.write_bit(true); // general_progressive_source_flag
    w.write_bit(false); // general_interlaced_source_flag
    w.write_bit(true); // general_non_packed_constraint_flag
    w.write_bit(true); // general_frame_only_constraint_flag
    match general_profile_idc {
        4..=10 => {
            // Format Range Extensions ("Rext") family: nine explicit
            // constraint flags, set from this profile's *actual*
            // chroma/bit-depth ceiling -- not copied from any real
            // encoder's stream, since declaring itself accurately is this
            // probe's only job.
            w.write_bit(profile.bit_depth <= 12); // general_max_12bit_constraint_flag
            w.write_bit(profile.bit_depth <= 10); // general_max_10bit_constraint_flag
            w.write_bit(profile.bit_depth <= 8); // general_max_8bit_constraint_flag
            w.write_bit(profile.chroma_format_idc <= 2); // general_max_422chroma_constraint_flag
            w.write_bit(profile.chroma_format_idc <= 1); // general_max_420chroma_constraint_flag
            w.write_bit(false); // general_max_monochrome_constraint_flag
            w.write_bit(false); // general_intra_constraint_flag
            w.write_bit(false); // general_one_picture_only_constraint_flag
            w.write_bit(false); // general_lower_bit_rate_constraint_flag
            w.write_zeros(34); // XXX_reserved_zero_34bits[0..33]
        }
        2 => {
            w.write_zeros(7);
            w.write_bit(false); // general_one_picture_only_constraint_flag
            w.write_zeros(35); // XXX_reserved_zero_35bits[0..34]
        }
        _ => w.write_zeros(43), // XXX_reserved_zero_43bits[0..42] (Main / Main Still Picture)
    }
    w.write_bit(false); // general_inbld_flag / general_reserved_zero_bit
    w.write_bits(u32::from(PROBE_HEVC_LEVEL_IDC), 8);
}

/// Prepends a two-byte HEVC NAL header (`nuh_layer_id = 0`,
/// `nuh_temporal_id_plus1 = 1`) and Annex-B-escapes `rbsp`.
#[must_use]
pub fn hevc_nal(nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let escaped = escape_emulation_prevention(rbsp);
    let mut nal = Vec::with_capacity(2 + escaped.len());
    nal.push(nal_unit_type << 1);
    nal.push(1);
    nal.extend_from_slice(&escaped);
    nal
}

/// Builds a complete, spec-valid `video_parameter_set_rbsp` NAL for
/// `profile`: single layer, single sub-layer, no timing/extension data --
/// the minimum `CMVideoFormatDescriptionCreateFromHEVCParameterSets`
/// needs a VPS for at all.
#[must_use]
pub fn build_hevc_vps(profile: HevcProbeProfile) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0, 4); // vps_video_parameter_set_id
    w.write_bit(true); // vps_base_layer_internal_flag
    w.write_bit(true); // vps_base_layer_available_flag
    w.write_bits(0, 6); // vps_max_layers_minus1
    w.write_bits(0, 3); // vps_max_sub_layers_minus1
    w.write_bit(true); // vps_temporal_id_nesting_flag
    w.write_bits(0xFFFF, 16); // vps_reserved_0xffff_16bits
    write_profile_tier_level(&mut w, profile);
    w.write_bit(false); // vps_sub_layer_ordering_info_present_flag
    w.write_ue(0); // vps_max_dec_pic_buffering_minus1[0]
    w.write_ue(0); // vps_max_num_reorder_pics[0]
    w.write_ue(0); // vps_max_latency_increase_plus1[0]
    w.write_bits(0, 6); // vps_max_layer_id
    w.write_ue(0); // vps_num_layer_sets_minus1
    w.write_bit(false); // vps_timing_info_present_flag
    w.write_bit(false); // vps_extension_flag
    w.rbsp_trailing_bits();
    hevc_nal(32, &w.into_rbsp())
}

/// Builds a complete `seq_parameter_set_rbsp` NAL for `profile` at a
/// fixed `PROBE_DIMENSION` x `PROBE_DIMENSION` picture: one CTU, no
/// scaling lists, no PCM, no reference-picture sets, no VUI, no SPS
/// extension -- nothing this probe's capability question (does
/// `chroma_format_idc`/`bit_depth_*_minus8` under this
/// `general_profile_idc` even build a session) depends on.
#[must_use]
pub fn build_hevc_sps(profile: HevcProbeProfile) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0, 4); // sps_video_parameter_set_id
    w.write_bits(0, 3); // sps_max_sub_layers_minus1
    w.write_bit(true); // sps_temporal_id_nesting_flag
    write_profile_tier_level(&mut w, profile);
    w.write_ue(0); // sps_seq_parameter_set_id
    w.write_ue(u32::from(profile.chroma_format_idc)); // chroma_format_idc
    if profile.chroma_format_idc == 3 {
        w.write_bit(false); // separate_colour_plane_flag
    }
    w.write_ue(u32::from(PROBE_DIMENSION)); // pic_width_in_luma_samples
    w.write_ue(u32::from(PROBE_DIMENSION)); // pic_height_in_luma_samples
    w.write_bit(false); // conformance_window_flag
    w.write_ue(u32::from(profile.bit_depth - 8)); // bit_depth_luma_minus8
    w.write_ue(u32::from(profile.bit_depth - 8)); // bit_depth_chroma_minus8
    w.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
    w.write_bit(false); // sps_sub_layer_ordering_info_present_flag
    w.write_ue(0); // sps_max_dec_pic_buffering_minus1[0]
    w.write_ue(0); // sps_max_num_reorder_pics[0]
    w.write_ue(0); // sps_max_latency_increase_plus1[0]
    w.write_ue(0); // log2_min_luma_coding_block_size_minus3 (min CB = 8)
    w.write_ue(3); // log2_diff_max_min_luma_coding_block_size (CTU = 64)
    w.write_ue(0); // log2_min_luma_transform_block_size_minus2 (min TB = 4)
    w.write_ue(3); // log2_diff_max_min_transform_block_size (max TB = 32)
    w.write_ue(0); // max_transform_hierarchy_depth_inter
    w.write_ue(0); // max_transform_hierarchy_depth_intra
    w.write_bit(false); // scaling_list_enabled_flag
    w.write_bit(false); // amp_enabled_flag
    w.write_bit(false); // sample_adaptive_offset_enabled_flag
    w.write_bit(false); // pcm_enabled_flag
    w.write_ue(0); // num_short_term_ref_pic_sets
    w.write_bit(false); // long_term_ref_pics_present_flag
    w.write_bit(false); // sps_temporal_mvp_enabled_flag
    w.write_bit(false); // strong_intra_smoothing_enabled_flag
    w.write_bit(false); // vui_parameters_present_flag
    w.write_bit(false); // sps_extension_present_flag
    w.rbsp_trailing_bits();
    hevc_nal(33, &w.into_rbsp())
}

/// Builds a `pic_parameter_set_rbsp` NAL. Profile-independent: nothing
/// this probe cares about (chroma/bit-depth/profile) lives in the PPS.
#[must_use]
pub fn build_hevc_pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_ue(0); // pps_pic_parameter_set_id
    w.write_ue(0); // pps_seq_parameter_set_id
    w.write_bit(false); // dependent_slice_segments_enabled_flag
    w.write_bit(false); // output_flag_present_flag
    w.write_bits(0, 3); // num_extra_slice_header_bits
    w.write_bit(false); // sign_data_hiding_enabled_flag
    w.write_bit(false); // cabac_init_present_flag
    w.write_ue(0); // num_ref_idx_l0_default_active_minus1
    w.write_ue(0); // num_ref_idx_l1_default_active_minus1
    w.write_se(0); // init_qp_minus26
    w.write_bit(false); // constrained_intra_pred_flag
    w.write_bit(false); // transform_skip_enabled_flag
    w.write_bit(false); // cu_qp_delta_enabled_flag
    w.write_se(0); // pps_cb_qp_offset
    w.write_se(0); // pps_cr_qp_offset
    w.write_bit(false); // pps_slice_chroma_qp_offsets_present_flag
    w.write_bit(false); // weighted_pred_flag
    w.write_bit(false); // weighted_bipred_flag
    w.write_bit(false); // transquant_bypass_enabled_flag
    w.write_bit(false); // tiles_enabled_flag
    w.write_bit(false); // entropy_coding_sync_enabled_flag
    w.write_bit(true); // pps_loop_filter_across_slices_enabled_flag
    w.write_bit(false); // deblocking_filter_control_present_flag
    w.write_bit(false); // pps_scaling_list_data_present_flag
    w.write_bit(false); // lists_modification_present_flag
    w.write_ue(0); // log2_parallel_merge_level_minus2
    w.write_bit(false); // slice_segment_header_extension_present_flag
    w.write_bit(false); // pps_extension_present_flag
    w.rbsp_trailing_bits();
    hevc_nal(34, &w.into_rbsp())
}

/// Prepends a one-byte H.264 NAL header (`nal_ref_idc = 3`, the
/// conventional highest-importance value for parameter sets) and
/// Annex-B-escapes `rbsp`.
#[must_use]
pub fn h264_nal(nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let escaped = escape_emulation_prevention(rbsp);
    let mut nal = Vec::with_capacity(1 + escaped.len());
    nal.push((3 << 5) | nal_unit_type);
    nal.extend_from_slice(&escaped);
    nal
}

/// Builds a minimal Baseline-profile `seq_parameter_set_rbsp` NAL: fixed
/// `PROBE_DIMENSION` x `PROBE_DIMENSION`, implicitly 4:2:0 eight-bit (the
/// high-profile-only chroma/bit-depth fields do not exist in a Baseline
/// SPS at all -- ITU-T H.264 7.3.2.1.1 gates them on `profile_idc`).
/// `pic_order_cnt_type = 2` sidesteps every optional POC field.
#[must_use]
pub fn build_h264_sps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(66, 8); // profile_idc = Baseline
    w.write_bit(true); // constraint_set0_flag (obeys Annex A.2.1, Baseline)
    w.write_zeros(5); // constraint_set1_flag .. constraint_set5_flag
    w.write_bits(0, 2); // reserved_zero_2bits
    w.write_bits(10, 8); // level_idc = 1.0
    w.write_ue(0); // seq_parameter_set_id
    w.write_ue(0); // log2_max_frame_num_minus4
    w.write_ue(2); // pic_order_cnt_type = 2 (sidesteps every optional POC field)
    w.write_ue(1); // max_num_ref_frames
    w.write_bit(false); // gaps_in_frame_num_value_allowed_flag
    w.write_ue(u32::from(PROBE_DIMENSION / 16 - 1)); // pic_width_in_mbs_minus1
    w.write_ue(u32::from(PROBE_DIMENSION / 16 - 1)); // pic_height_in_map_units_minus1
    w.write_bit(true); // frame_mbs_only_flag
    w.write_bit(true); // direct_8x8_inference_flag
    w.write_bit(false); // frame_cropping_flag
    w.write_bit(false); // vui_parameters_present_flag
    w.rbsp_trailing_bits();
    h264_nal(7, &w.into_rbsp())
}

/// Builds a minimal `pic_parameter_set_rbsp` NAL (CAVLC, one slice
/// group).
#[must_use]
pub fn build_h264_pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_ue(0); // pic_parameter_set_id
    w.write_ue(0); // seq_parameter_set_id
    w.write_bit(false); // entropy_coding_mode_flag (CAVLC)
    w.write_bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.write_ue(0); // num_slice_groups_minus1
    w.write_ue(0); // num_ref_idx_l0_default_active_minus1
    w.write_ue(0); // num_ref_idx_l1_default_active_minus1
    w.write_bit(false); // weighted_pred_flag
    w.write_bits(0, 2); // weighted_bipred_idc
    w.write_se(0); // pic_init_qp_minus26
    w.write_se(0); // pic_init_qs_minus26
    w.write_se(0); // chroma_qp_index_offset
    w.write_bit(false); // deblocking_filter_control_present_flag
    w.write_bit(false); // constrained_intra_pred_flag
    w.write_bit(false); // redundant_pic_cnt_present_flag
    w.rbsp_trailing_bits();
    h264_nal(8, &w.into_rbsp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstream::{BitReader, unescape_rbsp};

    #[test]
    fn hevc_nal_headers_match_the_canonical_two_byte_prefixes() {
        let profile = HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 8,
        };
        assert_eq!(&build_hevc_vps(profile)[..2], &[0x40, 0x01]);
        assert_eq!(&build_hevc_sps(profile)[..2], &[0x42, 0x01]);
        assert_eq!(&build_hevc_pps()[..2], &[0x44, 0x01]);
    }

    #[test]
    fn h264_nal_headers_match_the_canonical_sps_pps_prefixes() {
        assert_eq!(build_h264_sps()[0], 0x67);
        assert_eq!(build_h264_pps()[0], 0x68);
    }

    #[test]
    fn hevc_general_profile_idc_matches_x265_style_profile_selection() {
        let main = HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 8,
        };
        let main10 = HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 10,
        };
        let main12 = HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 12,
        };
        let yuv444_8 = HevcProbeProfile {
            chroma_format_idc: 3,
            bit_depth: 8,
        };
        assert_eq!(hevc_general_profile_idc(main), 1);
        assert_eq!(hevc_general_profile_idc(main10), 2);
        assert_eq!(hevc_general_profile_idc(main12), 4);
        assert_eq!(hevc_general_profile_idc(yuv444_8), 4);
    }

    #[test]
    fn hevc_sps_round_trips_profile_chroma_and_bit_depth_for_every_probed_profile() {
        // Independently re-parses (not via the writer) each SPS the real
        // probe builds, confirming the fields VideoToolbox actually keys
        // decodability on land where the builder above believes they do.
        let profiles = [
            HevcProbeProfile {
                chroma_format_idc: 1,
                bit_depth: 8,
            },
            HevcProbeProfile {
                chroma_format_idc: 1,
                bit_depth: 10,
            },
            HevcProbeProfile {
                chroma_format_idc: 1,
                bit_depth: 12,
            },
            HevcProbeProfile {
                chroma_format_idc: 3,
                bit_depth: 8,
            },
        ];
        for profile in profiles {
            let sps = build_hevc_sps(profile);
            let rbsp = unescape_rbsp(&sps[2..]);
            let mut r = BitReader::new(&rbsp);
            r.skip(4).unwrap(); // sps_video_parameter_set_id
            r.skip(3).unwrap(); // sps_max_sub_layers_minus1
            r.skip(1).unwrap(); // sps_temporal_id_nesting_flag
            r.skip(3).unwrap(); // general_profile_space + general_tier_flag
            let general_profile_idc = r.u(5).unwrap();
            assert_eq!(
                general_profile_idc,
                u32::from(hevc_general_profile_idc(profile)),
                "{profile:?}"
            );
            r.skip(32).unwrap(); // general_profile_compatibility_flag[32]
            r.skip(4).unwrap(); // progressive/interlaced/non_packed/frame_only source flags
            match general_profile_idc {
                4..=10 => r.skip(9 + 34).unwrap(),
                2 => r.skip(7 + 1 + 35).unwrap(),
                _ => r.skip(43).unwrap(),
            }
            r.skip(1).unwrap(); // general_inbld_flag / reserved
            r.skip(8).unwrap(); // general_level_idc
            r.ue().unwrap(); // sps_seq_parameter_set_id
            let chroma_format_idc = r.ue().unwrap();
            assert_eq!(
                chroma_format_idc,
                u32::from(profile.chroma_format_idc),
                "{profile:?}"
            );
            if profile.chroma_format_idc == 3 {
                r.skip(1).unwrap(); // separate_colour_plane_flag
            }
            assert_eq!(r.ue().unwrap(), u32::from(PROBE_DIMENSION)); // pic_width_in_luma_samples
            assert_eq!(r.ue().unwrap(), u32::from(PROBE_DIMENSION)); // pic_height_in_luma_samples
            r.skip(1).unwrap(); // conformance_window_flag
            let bit_depth_luma_minus8 = r.ue().unwrap();
            let bit_depth_chroma_minus8 = r.ue().unwrap();
            assert_eq!(
                bit_depth_luma_minus8,
                u32::from(profile.bit_depth - 8),
                "{profile:?}"
            );
            assert_eq!(
                bit_depth_chroma_minus8,
                u32::from(profile.bit_depth - 8),
                "{profile:?}"
            );
        }
    }
}
