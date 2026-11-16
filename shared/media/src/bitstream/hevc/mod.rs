// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! H.265 / HEVC: parameter sets, slice headers, picture order count,
//! reference picture sets, the decoded picture buffer and reference picture
//! lists (ITU-T H.265 §7, §8.3, §C.5.2), producing a decoder-independent
//! [`HevcPicture`] per access unit for a hardware decoder adapter.
//!
//! Scope: single-layer streams (Main, Main 10, Main Still and the format
//! range extensions such as Main 4:4:4 10). Multilayer, 3D and screen-content
//! extensions are refused. A picture whose reference is missing is refused
//! too (never concealed) and the parser waits for the next IRAP picture.

pub mod params;
pub mod slice;

use std::ops::Range;
use std::sync::Arc;

use super::picture::{OutputPicture, Slot, SlotPool, StreamCodecKind, StreamFormat};
use super::{BitstreamError, unescape_rbsp};
use crate::annexb::nal_units;
pub use params::{Pps, ScalingList, ShortTermRps, Sps, Vps};
pub use slice::{SliceHeader, SliceType};

/// `nal_unit_type` values (Table 7-1).
pub mod nal_type {
    pub const TRAIL_N: u8 = 0;
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const RSV_VCL_N14: u8 = 14;
    pub const BLA_W_LP: u8 = 16;
    pub const BLA_N_LP: u8 = 18;
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA_NUT: u8 = 21;
    pub const RSV_IRAP_23: u8 = 23;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const EOS: u8 = 36;
    pub const EOB: u8 = 37;
}

/// The two-byte NAL unit header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalHeader {
    pub unit_type: u8,
    pub layer_id: u8,
    pub temporal_id: u8,
}

impl NalHeader {
    /// Reads the header at the start of `nal`.
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] for a short unit, the forbidden bit, or
    /// `nuh_temporal_id_plus1 == 0`.
    pub fn parse(nal: &[u8]) -> Result<Self, BitstreamError> {
        let [first, second, ..] = *nal else {
            return Err(BitstreamError::Truncated);
        };
        if first & 0x80 != 0 {
            return Err(BitstreamError::Invalid("forbidden_zero_bit"));
        }
        let temporal_plus1 = second & 0x07;
        if temporal_plus1 == 0 {
            return Err(BitstreamError::Invalid("nuh_temporal_id_plus1"));
        }
        Ok(Self {
            unit_type: (first >> 1) & 0x3f,
            layer_id: ((first & 1) << 5) | (second >> 3),
            temporal_id: temporal_plus1 - 1,
        })
    }

    #[must_use]
    pub const fn is_vcl(self) -> bool {
        self.unit_type < 32
    }

    #[must_use]
    pub const fn is_irap(self) -> bool {
        self.unit_type >= nal_type::BLA_W_LP && self.unit_type <= nal_type::RSV_IRAP_23
    }

    #[must_use]
    pub const fn is_idr(self) -> bool {
        self.unit_type == nal_type::IDR_W_RADL || self.unit_type == nal_type::IDR_N_LP
    }

    #[must_use]
    pub const fn is_bla(self) -> bool {
        self.unit_type >= nal_type::BLA_W_LP && self.unit_type <= nal_type::BLA_N_LP
    }

    #[must_use]
    pub const fn is_rasl(self) -> bool {
        self.unit_type == nal_type::RASL_N || self.unit_type == nal_type::RASL_R
    }

    /// RADL or RASL (leading pictures).
    #[must_use]
    pub const fn is_leading(self) -> bool {
        self.unit_type >= 6 && self.unit_type <= nal_type::RASL_R
    }

    /// A sub-layer non-reference picture (`*_N` types below 16).
    #[must_use]
    pub const fn is_sub_layer_non_reference(self) -> bool {
        self.unit_type <= nal_type::RSV_VCL_N14 && self.unit_type % 2 == 0
    }
}

/// A reference picture of the current picture, as a hardware decoder needs
/// it (`RefPicList`/`PicOrderCntValList` in DXVA terms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HevcReference {
    pub slot: Slot,
    pub poc: i32,
    pub long_term: bool,
}

/// One slice segment of the current picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcSlice {
    /// The NAL unit (header included, no start code) inside the access unit
    /// passed to [`HevcParser::decode_access_unit`].
    pub nal: Range<usize>,
    pub header: SliceHeader,
    /// `RefPicList0`/`RefPicList1` as indices into
    /// [`HevcPicture::references`].
    pub ref_list0: Vec<u8>,
    pub ref_list1: Vec<u8>,
}

/// Everything a hardware decoder needs to decode one picture.
#[derive(Debug, Clone)]
pub struct HevcPicture {
    pub sps: Arc<Sps>,
    pub pps: Arc<Pps>,
    pub slot: Slot,
    pub poc: i32,
    pub nal_unit_type: u8,
    pub irap: bool,
    pub idr: bool,
    /// Every slice is an I slice.
    pub intra_only: bool,
    /// Every picture still marked as a reference (current excluded), at most
    /// 15.
    pub references: Vec<HevcReference>,
    /// `RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr` as indices into
    /// [`Self::references`].
    pub st_curr_before: Vec<u8>,
    pub st_curr_after: Vec<u8>,
    pub lt_curr: Vec<u8>,
    /// From the first slice: `NumDeltaPocs[RefRpsIdx]` of a predicted RPS
    /// coded in the slice header, else 0.
    pub num_delta_pocs_of_ref_rps_idx: u8,
    /// Bits of the RPS coded in the first slice header, else 0.
    pub short_term_rps_bits: u32,
    /// Resolved tile column widths and row heights in CTBs, minus one, for
    /// every column and row but the last (at most 19 and 21 entries, the
    /// `column_width_minus1`/`row_height_minus1` of DXVA).
    pub tile_columns_minus1: Vec<u16>,
    pub tile_rows_minus1: Vec<u16>,
    /// The scaling lists in force (PPS over SPS), when enabled.
    pub scaling_list: Option<ScalingList>,
    pub slices: Vec<HevcSlice>,
}

/// What one access unit produced.
#[derive(Debug, Clone)]
pub struct HevcAccessUnit {
    /// The stream format changed with this unit: rebuild the hardware
    /// decoder before decoding [`Self::picture`].
    pub format_changed: bool,
    pub format: Option<StreamFormat>,
    /// The picture to decode now, if the unit carried one that is decodable.
    pub picture: Option<HevcPicture>,
    /// Pictures to present now, in output order (after decoding
    /// [`Self::picture`], which may be among them).
    pub output: Vec<OutputPicture>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marking {
    Unused,
    ShortTerm,
    LongTerm,
}

#[derive(Debug, Clone, Copy)]
struct DpbEntry {
    slot: Slot,
    poc: i32,
    marking: Marking,
    needed_for_output: bool,
    latency: u32,
}

/// The H.265 picture manager: feed it access units, decode what it returns.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct HevcParser {
    vps: Vec<Option<Arc<Vps>>>,
    sps: Vec<Option<Arc<Sps>>>,
    pps: Vec<Option<Arc<Pps>>>,
    active_sps: Option<Arc<Sps>>,
    format: Option<StreamFormat>,
    extra_slots: u8,
    pool: SlotPool,
    dpb: Vec<DpbEntry>,
    /// The next picture starts a coded video sequence (stream start, after
    /// end of sequence, or after a reset).
    first_picture: bool,
    /// Decoding is suspended until an IRAP picture (after a reset or a
    /// missing reference).
    waiting_for_irap: bool,
    /// Whether any picture has been decoded (picture 0 has no prior
    /// pictures to output or drop).
    decoded_any: bool,
    /// `NoRaslOutputFlag` of the last IRAP picture.
    irap_no_rasl_output: bool,
    prev_tid0_poc: i32,
}

impl HevcParser {
    /// A parser whose slot pool leaves `extra_slots` beyond the stream's
    /// decoded picture buffer for pictures the caller still displays.
    #[must_use]
    pub fn new(extra_slots: u8) -> Self {
        Self {
            vps: vec![None; 16],
            sps: vec![None; 16],
            pps: vec![None; 64],
            active_sps: None,
            format: None,
            extra_slots,
            pool: SlotPool::default(),
            dpb: Vec::new(),
            first_picture: true,
            waiting_for_irap: true,
            decoded_any: false,
            irap_no_rasl_output: true,
            prev_tid0_poc: 0,
        }
    }

    /// Forgets every decoded picture; decoding resumes at the next IRAP
    /// picture. Parameter sets are kept.
    pub fn reset(&mut self) {
        self.dpb.clear();
        self.first_picture = true;
        self.waiting_for_irap = true;
    }

    /// The slot pool size the current format needs (DPB plus extras).
    #[must_use]
    pub const fn pool_size(&self) -> u8 {
        self.pool.size()
    }

    /// Whether `slot` holds a picture the parser still needs.
    #[must_use]
    pub fn slot_in_use(&self, slot: Slot) -> bool {
        self.dpb.iter().any(|entry| entry.slot == slot)
    }

    /// Outputs every picture still waiting, in output order (end of stream).
    pub fn flush(&mut self) -> Vec<OutputPicture> {
        let mut output = Vec::new();
        while self.bump(&mut output) {}
        self.dpb.clear();
        output
    }

    /// Parses one Annex B access unit. `held` reports slots the caller is
    /// still displaying, which are not reused.
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] for malformed or unsupported syntax, or a missing
    /// reference. After an error the parser waits for an IRAP picture; the
    /// caller asks the sender for a keyframe.
    pub fn decode_access_unit(
        &mut self,
        access_unit: &[u8],
        held: &dyn Fn(Slot) -> bool,
    ) -> Result<HevcAccessUnit, BitstreamError> {
        let result = self.decode_inner(access_unit, held);
        if result.is_err() {
            // Restart at the next IRAP as if it began the stream
            // (HandleCraAsBlaFlag): its leading pictures are skipped.
            self.waiting_for_irap = true;
            self.first_picture = true;
        }
        result
    }

    fn decode_inner(
        &mut self,
        access_unit: &[u8],
        held: &dyn Fn(Slot) -> bool,
    ) -> Result<HevcAccessUnit, BitstreamError> {
        let base = access_unit.as_ptr() as usize;
        let mut slices: Vec<(Range<usize>, NalHeader, SliceHeader)> = Vec::new();
        let mut picture_sets: Option<(Arc<Pps>, Arc<Sps>)> = None;
        let mut picture_nal: Option<NalHeader> = None;
        // An end of sequence closes the unit of the picture before it; it
        // applies to the next picture.
        let mut end_of_sequence = false;
        for nal in nal_units(access_unit) {
            let header = NalHeader::parse(nal)?;
            if header.layer_id != 0 {
                continue;
            }
            match header.unit_type {
                nal_type::VPS => {
                    let vps = Vps::parse(&unescape_rbsp(&nal[2..]))?;
                    let id = usize::from(vps.id);
                    self.vps[id] = Some(Arc::new(vps));
                }
                nal_type::SPS => {
                    let sps = Sps::parse(&unescape_rbsp(&nal[2..]))?;
                    let id = usize::from(sps.id);
                    self.sps[id] = Some(Arc::new(sps));
                }
                nal_type::PPS => {
                    let rbsp = unescape_rbsp(&nal[2..]);
                    let chroma_444 = peek_pps_sps_id(&rbsp)
                        .and_then(|id| self.sps.get(id).cloned().flatten())
                        .is_some_and(|sps| sps.chroma_format_idc == 3);
                    let pps = Pps::parse(&rbsp, chroma_444)?;
                    let id = usize::from(pps.id);
                    self.pps[id] = Some(Arc::new(pps));
                }
                nal_type::EOS | nal_type::EOB => end_of_sequence = true,
                unit if header.is_vcl() => {
                    if (22..=31).contains(&unit) || (10..=15).contains(&unit) {
                        return Err(BitstreamError::Invalid("reserved VCL nal_unit_type"));
                    }
                    let rbsp = unescape_rbsp(&nal[2..]);
                    let previous = slices.last().map(|(_, _, header)| header);
                    let (slice, pps, sps) =
                        SliceHeader::parse(&rbsp, header, |id| self.lookup(id), previous)?;
                    if slice.first_slice_segment_in_pic_flag != slices.is_empty() {
                        return Err(if slices.is_empty() {
                            BitstreamError::Invalid("first_slice_segment_in_pic_flag")
                        } else {
                            BitstreamError::Unsupported("several pictures in one access unit")
                        });
                    }
                    if let Some(first) = picture_nal {
                        if first.unit_type != header.unit_type {
                            return Err(BitstreamError::Invalid("nal_unit_type within a picture"));
                        }
                    }
                    if let Some((first_pps, _)) = &picture_sets {
                        if first_pps.id != pps.id {
                            return Err(BitstreamError::Invalid("PPS within a picture"));
                        }
                    }
                    picture_nal = Some(header);
                    picture_sets = Some((pps, sps));
                    let start = nal.as_ptr() as usize - base;
                    slices.push((start..start + nal.len(), header, slice));
                }
                _ => {}
            }
        }
        let (Some((pps, sps)), Some(nal)) = (picture_sets, picture_nal) else {
            self.first_picture |= end_of_sequence;
            return Ok(HevcAccessUnit {
                format_changed: false,
                format: self.format,
                picture: None,
                output: Vec::new(),
            });
        };
        let result = self.decode_picture(nal, &pps, &sps, slices, held);
        self.first_picture |= end_of_sequence;
        result
    }

    fn lookup(&self, pps_id: u8) -> Option<(Arc<Pps>, Arc<Sps>)> {
        let pps = self.pps.get(usize::from(pps_id))?.clone()?;
        let sps = self.sps.get(usize::from(pps.sps_id))?.clone()?;
        Some((pps, sps))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_picture(
        &mut self,
        nal: NalHeader,
        pps: &Arc<Pps>,
        sps: &Arc<Sps>,
        slices: Vec<(Range<usize>, NalHeader, SliceHeader)>,
        held: &dyn Fn(Slot) -> bool,
    ) -> Result<HevcAccessUnit, BitstreamError> {
        pps.validate_against(sps)?;
        let mut output = Vec::new();
        let mut format_changed = false;
        if self.active_sps.as_deref() != Some(sps.as_ref()) {
            if !nal.is_irap() {
                return Err(BitstreamError::Invalid(
                    "SPS activated by a non-IRAP picture",
                ));
            }
            let format = stream_format(sps);
            if self.format != Some(format) {
                format_changed = true;
                self.format = Some(format);
                self.pool = SlotPool::new(format.dpb_slots.saturating_add(self.extra_slots));
                // A new format means new surfaces; pictures of the old one
                // cannot be shown (NoOutputOfPriorPicsFlag may be inferred).
                self.dpb.clear();
            }
            self.active_sps = Some(Arc::clone(sps));
        }
        if self.waiting_for_irap && !nal.is_irap() {
            return Err(BitstreamError::MissingReference);
        }
        let first_header = &slices[0].2;
        let max_lsb = 1_i32 << sps.log2_max_pic_order_cnt_lsb;
        let no_rasl_output = nal.is_irap() && (nal.is_idr() || nal.is_bla() || self.first_picture);
        if nal.is_irap() {
            self.irap_no_rasl_output = no_rasl_output;
        }
        if nal.is_rasl() && self.irap_no_rasl_output {
            // Its references precede the IRAP picture we started from.
            return Ok(HevcAccessUnit {
                format_changed,
                format: self.format,
                picture: None,
                output,
            });
        }

        // §8.3.1 picture order count.
        let lsb = i32::try_from(first_header.pic_order_cnt_lsb)
            .map_err(|_| BitstreamError::Invalid("slice_pic_order_cnt_lsb"))?;
        let msb = if nal.is_irap() && no_rasl_output {
            0
        } else {
            let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
            let prev_msb = self.prev_tid0_poc - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb.checked_add(max_lsb)
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb.checked_sub(max_lsb)
            } else {
                Some(prev_msb)
            }
            .ok_or(BitstreamError::Invalid("PicOrderCntVal"))?
        };
        let poc = msb
            .checked_add(lsb)
            .ok_or(BitstreamError::Invalid("PicOrderCntVal"))?;

        // §8.3.2 reference picture set and marking.
        let mut st_curr_before = Vec::new();
        let mut st_curr_after = Vec::new();
        let mut lt_curr = Vec::new();
        if nal.is_idr() || (nal.is_irap() && no_rasl_output) {
            for entry in &mut self.dpb {
                entry.marking = Marking::Unused;
            }
        } else {
            let mut keep_long = Vec::new();
            for entry in &first_header.long_term {
                let mut target = i64::from(entry.poc_lsb);
                if entry.msb_present {
                    target += i64::from(poc)
                        - i64::from(entry.delta_poc_msb_cycle) * i64::from(max_lsb)
                        - i64::from(poc & (max_lsb - 1));
                }
                let found = self.dpb.iter().position(|picture| {
                    picture.marking != Marking::Unused
                        && if entry.msb_present {
                            i64::from(picture.poc) == target
                        } else {
                            i64::from(picture.poc & (max_lsb - 1)) == target
                        }
                });
                match found {
                    Some(index) => {
                        keep_long.push(index);
                        if entry.used_by_curr {
                            lt_curr.push(index);
                        }
                    }
                    None if entry.used_by_curr => return Err(BitstreamError::MissingReference),
                    None => {}
                }
            }
            let mut keep_short = Vec::new();
            let rps = first_header.short_term_rps.clone().unwrap_or_default();
            let mut find_short = |delta: &i32, used: bool, list: &mut Vec<usize>| {
                let target = i64::from(poc) + i64::from(*delta);
                let found = self.dpb.iter().position(|picture| {
                    picture.marking == Marking::ShortTerm && i64::from(picture.poc) == target
                });
                match found {
                    Some(index) => {
                        keep_short.push(index);
                        if used {
                            list.push(index);
                        }
                        Ok(())
                    }
                    None if used => Err(BitstreamError::MissingReference),
                    None => Ok(()),
                }
            };
            for (delta, used) in rps.delta_poc_s0.iter().zip(&rps.used_s0) {
                find_short(delta, *used, &mut st_curr_before)?;
            }
            for (delta, used) in rps.delta_poc_s1.iter().zip(&rps.used_s1) {
                find_short(delta, *used, &mut st_curr_after)?;
            }
            for (index, entry) in self.dpb.iter_mut().enumerate() {
                entry.marking = if keep_long.contains(&index) {
                    Marking::LongTerm
                } else if keep_short.contains(&index) {
                    Marking::ShortTerm
                } else {
                    Marking::Unused
                };
            }
        }

        // §C.5.2.2 output and removal of pictures before decoding this one.
        let ordering = sps.highest_ordering();
        let dpb_capacity = usize::from(ordering.max_dec_pic_buffering_minus1) + 1;
        if nal.is_irap() && no_rasl_output && self.decoded_any {
            let no_output_of_prior_pics =
                nal.unit_type == nal_type::CRA_NUT || first_header.no_output_of_prior_pics_flag;
            if no_output_of_prior_pics {
                self.dpb.clear();
            } else {
                self.dpb
                    .retain(|entry| entry.needed_for_output || entry.marking != Marking::Unused);
                while self.bump(&mut output) {}
                self.dpb.clear();
            }
            st_curr_before.clear();
            st_curr_after.clear();
            lt_curr.clear();
        } else {
            // Indices into the DPB shift as pictures leave; key the lists by
            // slot until the references are rebuilt below.
            let before: Vec<Slot> = st_curr_before.iter().map(|i| self.dpb[*i].slot).collect();
            let after: Vec<Slot> = st_curr_after.iter().map(|i| self.dpb[*i].slot).collect();
            let long: Vec<Slot> = lt_curr.iter().map(|i| self.dpb[*i].slot).collect();
            self.dpb
                .retain(|entry| entry.needed_for_output || entry.marking != Marking::Unused);
            loop {
                let waiting = self.dpb.iter().filter(|e| e.needed_for_output).count();
                let latency_exceeded = self.latency_exceeded(ordering);
                if waiting > usize::from(ordering.max_num_reorder_pics)
                    || latency_exceeded
                    || self.dpb.len() >= dpb_capacity
                {
                    if !self.bump(&mut output) {
                        if self.dpb.len() >= dpb_capacity {
                            return Err(BitstreamError::Invalid("decoded picture buffer overflow"));
                        }
                        break;
                    }
                } else {
                    break;
                }
            }
            let index_of = |slot: &Slot| self.dpb.iter().position(|entry| entry.slot == *slot);
            st_curr_before = before.iter().filter_map(index_of).collect();
            st_curr_after = after.iter().filter_map(index_of).collect();
            lt_curr = long.iter().filter_map(index_of).collect();
        }

        // References as the hardware decoder sees them.
        let references: Vec<HevcReference> = self
            .dpb
            .iter()
            .filter(|entry| entry.marking != Marking::Unused)
            .map(|entry| HevcReference {
                slot: entry.slot,
                poc: entry.poc,
                long_term: entry.marking == Marking::LongTerm,
            })
            .collect();
        if references.len() > 15 {
            return Err(BitstreamError::Invalid("more than 15 reference pictures"));
        }
        let reference_index = |dpb_index: &usize| -> u8 {
            let slot = self.dpb[*dpb_index].slot;
            params::narrow(params::narrow_u32(
                references
                    .iter()
                    .position(|reference| reference.slot == slot)
                    .unwrap_or(0),
            ))
        };
        if st_curr_before.len() + st_curr_after.len() + lt_curr.len() > 8 {
            return Err(BitstreamError::Unsupported(
                "more than 8 current reference pictures",
            ));
        }
        let st_curr_before: Vec<u8> = st_curr_before.iter().map(reference_index).collect();
        let st_curr_after: Vec<u8> = st_curr_after.iter().map(reference_index).collect();
        let lt_curr: Vec<u8> = lt_curr.iter().map(reference_index).collect();

        let output_slots: Vec<Slot> = output
            .iter()
            .map(|picture: &OutputPicture| picture.slot)
            .collect();
        let slot = self.pool.allocate(
            |slot| self.slot_in_use(slot) || output_slots.contains(&slot),
            held,
        )?;

        let mut picture_slices = Vec::with_capacity(slices.len());
        let mut intra_only = true;
        for (range, _, header) in slices {
            intra_only &= header.slice_type == SliceType::I;
            let (ref_list0, ref_list1) =
                ref_lists(&header, &st_curr_before, &st_curr_after, &lt_curr)?;
            picture_slices.push(HevcSlice {
                nal: range,
                header,
                ref_list0,
                ref_list1,
            });
        }
        let first = picture_slices[0].header.clone();
        let (tile_columns_minus1, tile_rows_minus1) = match &pps.tiles {
            Some(tiles) => {
                let (mut columns, mut rows) = tiles.resolve(sps)?;
                // The last column and row follow from the picture size.
                columns.pop();
                rows.pop();
                (columns, rows)
            }
            None => (Vec::new(), Vec::new()),
        };
        let scaling_list = sps
            .scaling_list_enabled_flag
            .then(|| {
                pps.scaling_list
                    .clone()
                    .or_else(|| sps.scaling_list.clone())
            })
            .flatten();
        let picture = HevcPicture {
            sps: Arc::clone(sps),
            pps: Arc::clone(pps),
            slot,
            poc,
            nal_unit_type: nal.unit_type,
            irap: nal.is_irap(),
            idr: nal.is_idr(),
            intra_only,
            references,
            st_curr_before,
            st_curr_after,
            lt_curr,
            num_delta_pocs_of_ref_rps_idx: if first.short_term_rps_in_header {
                first
                    .short_term_rps
                    .as_ref()
                    .map_or(0, |rps| rps.num_delta_pocs_of_ref)
            } else {
                0
            },
            short_term_rps_bits: first.short_term_rps_bits,
            tile_columns_minus1,
            tile_rows_minus1,
            scaling_list,
            slices: picture_slices,
        };

        // §C.5.2.3 the current picture enters the DPB.
        let pic_output_flag = first.pic_output_flag;
        for entry in &mut self.dpb {
            if entry.needed_for_output {
                entry.latency = entry.latency.saturating_add(1);
            }
        }
        self.dpb.push(DpbEntry {
            slot,
            poc,
            marking: Marking::ShortTerm,
            needed_for_output: pic_output_flag,
            latency: 0,
        });
        loop {
            let waiting = self.dpb.iter().filter(|e| e.needed_for_output).count();
            let latency_exceeded = self.latency_exceeded(ordering);
            if (waiting > usize::from(ordering.max_num_reorder_pics) || latency_exceeded)
                && self.bump(&mut output)
            {
                continue;
            }
            break;
        }

        if nal.temporal_id == 0 && !nal.is_leading() && !nal.is_sub_layer_non_reference() {
            self.prev_tid0_poc = poc;
        }
        self.first_picture = false;
        self.waiting_for_irap = false;
        self.decoded_any = true;
        Ok(HevcAccessUnit {
            format_changed,
            format: self.format,
            picture: Some(picture),
            output,
        })
    }

    /// Whether a waiting picture reached `SpsMaxLatencyPictures`.
    fn latency_exceeded(&self, ordering: params::SubLayerOrdering) -> bool {
        if ordering.max_latency_increase_plus1 == 0 {
            return false;
        }
        let limit = u64::from(ordering.max_num_reorder_pics)
            + u64::from(ordering.max_latency_increase_plus1)
            - 1;
        self.dpb
            .iter()
            .any(|entry| entry.needed_for_output && u64::from(entry.latency) >= limit)
    }

    /// §C.5.2.4: outputs the waiting picture with the smallest POC. Returns
    /// whether one was output.
    fn bump(&mut self, output: &mut Vec<OutputPicture>) -> bool {
        let Some(index) = self
            .dpb
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.needed_for_output)
            .min_by_key(|(_, entry)| entry.poc)
            .map(|(index, _)| index)
        else {
            return false;
        };
        let entry = &mut self.dpb[index];
        entry.needed_for_output = false;
        output.push(OutputPicture {
            slot: entry.slot,
            poc: entry.poc,
        });
        if entry.marking == Marking::Unused {
            self.dpb.remove(index);
        }
        true
    }
}

/// §8.3.4 reference picture lists, as indices into the reference set.
fn ref_lists(
    header: &SliceHeader,
    st_curr_before: &[u8],
    st_curr_after: &[u8],
    lt_curr: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), BitstreamError> {
    if header.slice_type == SliceType::I {
        return Ok((Vec::new(), Vec::new()));
    }
    let total = st_curr_before.len() + st_curr_after.len() + lt_curr.len();
    if total == 0 || total != header.num_pic_total_curr() {
        return Err(BitstreamError::MissingReference);
    }
    let build = |active_minus1: u8, first: &[u8], second: &[u8], entries: Option<&Vec<u8>>| {
        let length = usize::from(active_minus1) + 1;
        let temp_length = length.max(total);
        let mut temp = Vec::with_capacity(temp_length);
        while temp.len() < temp_length {
            for list in [first, second, lt_curr] {
                for reference in list {
                    if temp.len() < temp_length {
                        temp.push(*reference);
                    }
                }
            }
        }
        (0..length)
            .map(|index| {
                let source = entries.map_or(index, |entries| usize::from(entries[index]));
                temp.get(source)
                    .copied()
                    .ok_or(BitstreamError::Invalid("list_entry"))
            })
            .collect::<Result<Vec<u8>, BitstreamError>>()
    };
    let list0 = build(
        header.num_ref_idx_l0_active_minus1,
        st_curr_before,
        st_curr_after,
        header.list_entry_l0.as_ref(),
    )?;
    let list1 = if header.slice_type == SliceType::B {
        build(
            header.num_ref_idx_l1_active_minus1,
            st_curr_after,
            st_curr_before,
            header.list_entry_l1.as_ref(),
        )?
    } else {
        Vec::new()
    };
    Ok((list0, list1))
}

/// The SPS id a PPS names, read without parsing the rest.
fn peek_pps_sps_id(rbsp: &[u8]) -> Option<usize> {
    let mut reader = super::BitReader::new(rbsp);
    reader.ue().ok()?;
    reader.ue().ok().map(|id| id as usize)
}

/// The decoder-session facts of an SPS.
#[must_use]
pub fn stream_format(sps: &Sps) -> StreamFormat {
    let (left, right, top, bottom) = sps.conformance_window;
    StreamFormat {
        codec: StreamCodecKind::H265,
        profile_idc: sps.profile_tier_level.profile_idc,
        coded_width: sps.pic_width_in_luma_samples,
        coded_height: sps.pic_height_in_luma_samples,
        visible: (
            left,
            top,
            sps.pic_width_in_luma_samples - left - right,
            sps.pic_height_in_luma_samples - top - bottom,
        ),
        chroma_format_idc: sps.chroma_format_idc,
        bit_depth_luma: sps.bit_depth_luma,
        bit_depth_chroma: sps.bit_depth_chroma,
        dpb_slots: sps.highest_ordering().max_dec_pic_buffering_minus1 + 1,
        colour: sps.vui.and_then(|vui| vui.colour),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstream::BitReader;
    use crate::bitstream::BitWriter;

    fn explicit_rps(negatives: &[(u32, bool)], positives: &[(u32, bool)]) -> ShortTermRps {
        let mut writer = BitWriter::new();
        writer.write_ue(u32::try_from(negatives.len()).unwrap());
        writer.write_ue(u32::try_from(positives.len()).unwrap());
        for (delta_minus1, used) in negatives.iter().chain(positives) {
            writer.write_ue(*delta_minus1);
            writer.write_bit(*used);
        }
        writer.rbsp_trailing_bits();
        let bytes = writer.into_rbsp();
        ShortTermRps::parse(&mut BitReader::new(&bytes), &[], false, 15).unwrap()
    }

    #[test]
    fn an_explicit_rps_accumulates_its_deltas() {
        let rps = explicit_rps(&[(0, true), (1, false)], &[(2, true)]);
        assert_eq!(rps.delta_poc_s0, vec![-1, -3]);
        assert_eq!(rps.used_s0, vec![true, false]);
        assert_eq!(rps.delta_poc_s1, vec![3]);
        assert_eq!(rps.num_used(), 2);
    }

    #[test]
    fn an_inter_predicted_rps_follows_equations_7_61_and_7_62() {
        // Reference set {-1, -3 | +3}; deltaRps = -1 shifts it to {-2, -4 | +2}
        // and adds deltaRps itself (-1) as a negative picture.
        let reference = explicit_rps(&[(0, true), (1, true)], &[(2, true)]);
        let mut writer = BitWriter::new();
        writer.write_bit(true); // inter_ref_pic_set_prediction_flag
        writer.write_bit(true); // delta_rps_sign
        writer.write_ue(0); // abs_delta_rps_minus1 -> deltaRps = -1
        for _ in 0..4 {
            writer.write_bit(true); // used_by_curr_pic_flag[j] for j = 0..=3
        }
        writer.rbsp_trailing_bits();
        let bytes = writer.into_rbsp();
        let rps =
            ShortTermRps::parse(&mut BitReader::new(&bytes), &[reference], false, 15).unwrap();
        assert_eq!(rps.delta_poc_s0, vec![-1, -2, -4]);
        assert_eq!(rps.delta_poc_s1, vec![2]);
        assert_eq!(rps.num_delta_pocs_of_ref, 3);
    }

    fn header(
        slice_type: SliceType,
        l0_minus1: u8,
        entries: Option<Vec<u8>>,
        used: usize,
    ) -> SliceHeader {
        let rps = ShortTermRps {
            delta_poc_s0: (1..=i32::try_from(used).unwrap())
                .map(|delta| -delta)
                .collect(),
            used_s0: vec![true; used],
            ..ShortTermRps::default()
        };
        SliceHeader {
            first_slice_segment_in_pic_flag: true,
            no_output_of_prior_pics_flag: false,
            pps_id: 0,
            dependent_slice_segment_flag: false,
            slice_segment_address: 0,
            slice_type,
            pic_output_flag: true,
            colour_plane_id: 0,
            pic_order_cnt_lsb: 0,
            short_term_rps: Some(rps),
            short_term_rps_in_header: false,
            short_term_rps_bits: 0,
            long_term: Vec::new(),
            slice_temporal_mvp_enabled_flag: false,
            slice_sao_luma_flag: false,
            slice_sao_chroma_flag: false,
            num_ref_idx_l0_active_minus1: l0_minus1,
            num_ref_idx_l1_active_minus1: 0,
            list_entry_l0: entries,
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
            deblocking_filter_disabled_flag: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            loop_filter_across_slices_enabled_flag: false,
            num_entry_point_offsets: 0,
        }
    }

    #[test]
    fn ref_lists_repeat_the_current_set_and_apply_list_entries() {
        // Two references, three active entries: the temporary list repeats.
        let (list0, _) = ref_lists(&header(SliceType::P, 2, None, 2), &[4, 7], &[], &[]).unwrap();
        assert_eq!(list0, vec![4, 7, 4]);
        let (list0, _) = ref_lists(
            &header(SliceType::P, 1, Some(vec![1, 1]), 2),
            &[4, 7],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(list0, vec![7, 7]);
        // A B slice's list 1 starts with the following pictures.
        let mut b = header(SliceType::B, 0, None, 1);
        b.short_term_rps.as_mut().unwrap().delta_poc_s1 = vec![1];
        b.short_term_rps.as_mut().unwrap().used_s1 = vec![true];
        let (list0, list1) = ref_lists(&b, &[2], &[5], &[]).unwrap();
        assert_eq!((list0, list1), (vec![2], vec![5]));
        // A reference that did not resolve is refused, not guessed.
        assert_eq!(
            ref_lists(&header(SliceType::P, 0, None, 2), &[4], &[], &[]),
            Err(BitstreamError::MissingReference)
        );
    }

    #[test]
    fn bumping_outputs_the_smallest_poc_and_frees_unused_pictures() {
        let mut parser = HevcParser::new(0);
        for (slot, poc, marking) in [
            (0, 8, Marking::ShortTerm),
            (1, 4, Marking::Unused),
            (2, 6, Marking::Unused),
        ] {
            parser.dpb.push(DpbEntry {
                slot: Slot(slot),
                poc,
                marking,
                needed_for_output: true,
                latency: 0,
            });
        }
        let output = parser.flush();
        assert_eq!(
            output.iter().map(|picture| picture.poc).collect::<Vec<_>>(),
            vec![4, 6, 8]
        );
        assert!(parser.dpb.is_empty());
    }

    #[test]
    fn a_non_irap_picture_after_a_reset_is_refused() {
        let mut parser = HevcParser::new(1);
        // A trailing picture's slice NAL (type 1) with nothing before it.
        let unit = [0, 0, 0, 1, 0x02, 0x01, 0x80];
        assert!(parser.decode_access_unit(&unit, &|_| false).is_err());
    }
}
