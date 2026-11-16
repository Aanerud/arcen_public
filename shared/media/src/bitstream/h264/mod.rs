// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! H.264 / AVC: parameter sets, slice headers, picture order count (types 0,
//! 1 and 2), reference marking (sliding window and MMCO), reference picture
//! lists with modification, and low-latency output (ITU-T H.264 §7, §8.2),
//! producing a decoder-independent [`H264Picture`] per access unit.
//!
//! Scope: progressive (frame) coding in the profiles a hardware decoder
//! exposes as H.264 VLD (Baseline/Main/High and the High 10/4:2:2/4:4:4
//! syntax). Field and MBAFF coding, slice groups, SP/SI slices and frame-num
//! gaps are refused with a reason. A missing reference is never concealed:
//! the parser waits for the next IDR picture.
//!
//! Output order: without `bitstream_restriction` an H.264 stream may reorder
//! up to a full DPB, which would cost a remote desktop many frames of
//! latency. Arcen's encoders never reorder, so pictures are output as soon as
//! no reordering is declared ([`super::hevc`] has the exact rule in its SPS);
//! a stream that reorders without declaring it is refused rather than shown
//! out of order.

pub mod params;
pub mod slice;

use std::ops::Range;
use std::sync::Arc;

use super::picture::{OutputPicture, Slot, SlotPool, StreamCodecKind, StreamFormat};
use super::{BitstreamError, unescape_rbsp};
use crate::annexb::nal_units;
pub use params::{PocType, Pps, ScalingMatrix, Sps};
pub use slice::{ListModification, Mmco, SliceHeader, SliceType};

/// A reference frame of the current picture, as a hardware decoder needs it
/// (`RefFrameList`/`FieldOrderCntList`/`FrameNumList` in DXVA terms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H264Reference {
    pub slot: Slot,
    pub long_term: bool,
    /// `FrameNum` of a short-term frame, `LongTermFrameIdx` of a long-term one.
    pub frame_num_or_long_term_idx: u32,
    pub top_poc: i32,
    pub bottom_poc: i32,
}

/// One slice of the current picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264Slice {
    /// The NAL unit (header included, no start code) inside the access unit.
    pub nal: Range<usize>,
    pub header: SliceHeader,
    /// `RefPicList0`/`RefPicList1` as indices into
    /// [`H264Picture::references`]; `None` where the list is longer than
    /// the references available ("no reference picture").
    pub ref_list0: Vec<Option<u8>>,
    pub ref_list1: Vec<Option<u8>>,
}

/// Everything a hardware decoder needs to decode one picture.
#[derive(Debug, Clone)]
pub struct H264Picture {
    pub sps: Arc<Sps>,
    pub pps: Arc<Pps>,
    pub slot: Slot,
    pub frame_num: u32,
    pub idr: bool,
    /// `nal_ref_idc != 0`.
    pub reference: bool,
    pub intra_only: bool,
    pub top_poc: i32,
    pub bottom_poc: i32,
    /// Every frame marked as a reference before this picture (at most 16).
    pub references: Vec<H264Reference>,
    pub slices: Vec<H264Slice>,
}

/// What one access unit produced.
#[derive(Debug, Clone)]
pub struct H264AccessUnit {
    pub format_changed: bool,
    pub format: Option<StreamFormat>,
    pub picture: Option<H264Picture>,
    pub output: Vec<OutputPicture>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marking {
    Unused,
    ShortTerm,
    LongTerm(u32),
}

#[derive(Debug, Clone, Copy)]
struct DpbFrame {
    slot: Slot,
    frame_num: u32,
    top_poc: i32,
    bottom_poc: i32,
    marking: Marking,
    needed_for_output: bool,
}

impl DpbFrame {
    fn poc(&self) -> i32 {
        self.top_poc.min(self.bottom_poc)
    }
}

/// State carried from the previous pictures for POC derivation (§8.2.1).
#[derive(Debug, Clone, Copy, Default)]
struct PreviousPicture {
    /// Previous reference picture: `PicOrderCntMsb` and `pic_order_cnt_lsb`
    /// (type 0).
    ref_msb: i32,
    ref_lsb: i32,
    /// Previous picture: `FrameNumOffset` and `frame_num` (types 1 and 2).
    frame_num_offset: i64,
    frame_num: u32,
}

/// The H.264 picture manager: feed it access units, decode what it returns.
#[derive(Debug)]
pub struct H264Parser {
    sps: Vec<Option<Arc<Sps>>>,
    /// Raw PPS RBSPs; parsed against their SPS when a slice uses them.
    pps_rbsp: Vec<Option<Vec<u8>>>,
    pps: Vec<Option<Arc<Pps>>>,
    active_sps: Option<Arc<Sps>>,
    format: Option<StreamFormat>,
    extra_slots: u8,
    pool: SlotPool,
    dpb: Vec<DpbFrame>,
    max_long_term_frame_idx: Option<u32>,
    previous: PreviousPicture,
    prev_ref_frame_num: u32,
    waiting_for_idr: bool,
    /// POC of the last output picture since the last IDR or MMCO 5, to catch
    /// undeclared reordering.
    last_output_poc: Option<i32>,
}

impl H264Parser {
    /// A parser whose slot pool leaves `extra_slots` beyond the stream's
    /// reference frames for pictures the caller still displays.
    #[must_use]
    pub fn new(extra_slots: u8) -> Self {
        Self {
            sps: vec![None; 32],
            pps_rbsp: vec![None; 256],
            pps: vec![None; 256],
            active_sps: None,
            format: None,
            extra_slots,
            pool: SlotPool::default(),
            dpb: Vec::new(),
            max_long_term_frame_idx: None,
            previous: PreviousPicture::default(),
            prev_ref_frame_num: 0,
            waiting_for_idr: true,
            last_output_poc: None,
        }
    }

    /// Forgets every decoded picture; decoding resumes at the next IDR.
    pub fn reset(&mut self) {
        self.dpb.clear();
        self.waiting_for_idr = true;
    }

    #[must_use]
    pub const fn pool_size(&self) -> u8 {
        self.pool.size()
    }

    #[must_use]
    pub fn slot_in_use(&self, slot: Slot) -> bool {
        self.dpb.iter().any(|frame| frame.slot == slot)
    }

    /// Outputs every picture still waiting, in output order.
    pub fn flush(&mut self) -> Vec<OutputPicture> {
        let mut output = Vec::new();
        while self.bump(&mut output) {}
        self.dpb.clear();
        output
    }

    /// Parses one Annex B access unit. `held` reports slots the caller is
    /// still displaying.
    ///
    /// # Errors
    ///
    /// [`BitstreamError`] for malformed or unsupported syntax or a missing
    /// reference; the parser then waits for an IDR picture and the caller
    /// asks the sender for a keyframe.
    pub fn decode_access_unit(
        &mut self,
        access_unit: &[u8],
        held: &dyn Fn(Slot) -> bool,
    ) -> Result<H264AccessUnit, BitstreamError> {
        let result = self.decode_inner(access_unit, held);
        if result.is_err() {
            self.waiting_for_idr = true;
        }
        result
    }

    fn pps_for(&mut self, id: u8) -> Result<(Arc<Pps>, Arc<Sps>), BitstreamError> {
        let index = usize::from(id);
        let missing = BitstreamError::MissingParameterSet {
            kind: "PPS",
            id: u32::from(id),
        };
        let rbsp = self.pps_rbsp[index].as_ref().ok_or(missing)?;
        let sps_id = Pps::peek_sps_id(rbsp).ok_or(BitstreamError::Truncated)?;
        let sps = self.sps.get(usize::from(sps_id)).cloned().flatten().ok_or(
            BitstreamError::MissingParameterSet {
                kind: "SPS",
                id: u32::from(sps_id),
            },
        )?;
        if let Some(pps) = &self.pps[index] {
            return Ok((Arc::clone(pps), sps));
        }
        let pps = Arc::new(Pps::parse(rbsp, &sps)?);
        self.pps[index] = Some(Arc::clone(&pps));
        Ok((pps, sps))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_inner(
        &mut self,
        access_unit: &[u8],
        held: &dyn Fn(Slot) -> bool,
    ) -> Result<H264AccessUnit, BitstreamError> {
        let base = access_unit.as_ptr() as usize;
        let mut slices: Vec<(Range<usize>, SliceHeader)> = Vec::new();
        let mut picture: Option<(Arc<Pps>, Arc<Sps>, u8, bool)> = None;
        for nal in nal_units(access_unit) {
            let header = nal[0];
            if header & 0x80 != 0 {
                return Err(BitstreamError::Invalid("forbidden_zero_bit"));
            }
            let nal_ref_idc = (header >> 5) & 3;
            match header & 0x1f {
                7 => {
                    let sps = Sps::parse(&unescape_rbsp(&nal[1..]))?;
                    let id = usize::from(sps.id);
                    if self.sps[id].as_deref() != Some(&sps) {
                        self.sps[id] = Some(Arc::new(sps));
                        // PPSs are resolved against their SPS: re-resolve.
                        self.pps.iter_mut().for_each(|pps| *pps = None);
                    }
                }
                8 => {
                    let rbsp = unescape_rbsp(&nal[1..]);
                    let id = {
                        let mut reader = super::BitReader::new(&rbsp);
                        reader.ue_max(255, "pic_parameter_set_id")?
                    };
                    let id = usize::try_from(id).map_err(|_| BitstreamError::Invalid("pps id"))?;
                    if self.pps_rbsp[id].as_deref() != Some(rbsp.as_slice()) {
                        self.pps_rbsp[id] = Some(rbsp);
                        self.pps[id] = None;
                    }
                }
                unit @ (1 | 5) => {
                    let idr = unit == 5;
                    if idr && nal_ref_idc == 0 {
                        return Err(BitstreamError::Invalid("nal_ref_idc of an IDR picture"));
                    }
                    let rbsp = unescape_rbsp(&nal[1..]);
                    let pps_id =
                        SliceHeader::peek_pps_id(&rbsp).ok_or(BitstreamError::Truncated)?;
                    let (pps, sps) = self.pps_for(pps_id)?;
                    let slice = SliceHeader::parse(&rbsp, nal_ref_idc, idr, &sps, &pps)?;
                    if slice.redundant_pic_cnt > 0 {
                        continue;
                    }
                    if let Some((first_pps, _, first_ref_idc, first_idr)) = &picture {
                        let first = &slices[0].1;
                        if slice.first_mb_in_slice == 0
                            || first.frame_num != slice.frame_num
                            || first.pic_order_cnt_lsb != slice.pic_order_cnt_lsb
                            || *first_idr != idr
                            || (*first_ref_idc == 0) != (nal_ref_idc == 0)
                        {
                            return Err(BitstreamError::Unsupported(
                                "several pictures in one access unit",
                            ));
                        }
                        if first_pps.id != pps.id {
                            return Err(BitstreamError::Invalid("PPS within a picture"));
                        }
                    }
                    picture.get_or_insert((pps, sps, nal_ref_idc, idr));
                    let start = nal.as_ptr() as usize - base;
                    slices.push((start..start + nal.len(), slice));
                }
                2..=4 => return Err(BitstreamError::Unsupported("H.264 data partitioning")),
                20 | 21 => return Err(BitstreamError::Unsupported("H.264 MVC/SVC")),
                _ => {}
            }
        }
        let Some((pps, sps, nal_ref_idc, idr)) = picture else {
            return Ok(H264AccessUnit {
                format_changed: false,
                format: self.format,
                picture: None,
                output: Vec::new(),
            });
        };
        self.decode_picture(&pps, &sps, nal_ref_idc, idr, slices, held)
    }

    #[allow(clippy::too_many_lines)]
    fn decode_picture(
        &mut self,
        pps: &Arc<Pps>,
        sps: &Arc<Sps>,
        nal_ref_idc: u8,
        idr: bool,
        slices: Vec<(Range<usize>, SliceHeader)>,
        held: &dyn Fn(Slot) -> bool,
    ) -> Result<H264AccessUnit, BitstreamError> {
        if !sps.frame_mbs_only_flag {
            return Err(BitstreamError::Unsupported("H.264 interlaced coding"));
        }
        let mut output = Vec::new();
        let mut format_changed = false;
        if self.active_sps.as_deref() != Some(sps.as_ref()) {
            if !idr {
                return Err(BitstreamError::Invalid(
                    "SPS activated by a non-IDR picture",
                ));
            }
            let format = stream_format(sps);
            if self.format != Some(format) {
                format_changed = true;
                self.format = Some(format);
                self.pool = SlotPool::new(format.dpb_slots.saturating_add(self.extra_slots));
                self.dpb.clear();
            }
            self.active_sps = Some(Arc::clone(sps));
        }
        if self.waiting_for_idr && !idr {
            return Err(BitstreamError::MissingReference);
        }
        let first = slices[0].1.clone();
        let max_frame_num = sps.max_frame_num();
        let reference = nal_ref_idc != 0;

        // §8.2.5.2: a gap in frame_num means lost reference pictures.
        if !idr
            && first.frame_num != self.prev_ref_frame_num
            && first.frame_num != (self.prev_ref_frame_num + 1) % max_frame_num
        {
            return Err(if sps.gaps_in_frame_num_value_allowed_flag {
                BitstreamError::Unsupported("H.264 frame_num gaps")
            } else {
                BitstreamError::MissingReference
            });
        }

        let (top_poc, bottom_poc, frame_num_offset) =
            self.picture_order_count(sps, &first, reference, idr)?;

        // §C.4.4 before the current picture: an IDR ends every earlier
        // reference; pictures still waiting are output first unless the
        // stream says not to. (MMCO 5 does the same, after decoding.)
        let has_mmco5 = first
            .marking
            .as_ref()
            .and_then(|marking| marking.adaptive.as_ref())
            .is_some_and(|ops| ops.contains(&Mmco::UnmarkAll));
        if idr {
            let drop_prior = first
                .marking
                .as_ref()
                .is_some_and(|marking| marking.no_output_of_prior_pics_flag);
            for frame in &mut self.dpb {
                frame.marking = Marking::Unused;
            }
            if !drop_prior {
                while self.bump(&mut output) {}
            }
            self.dpb.clear();
            self.last_output_poc = None;
        }
        // Pictures output before a sequence restart are not ordered against
        // the new sequence.
        let mut sequence_start = output.len();

        // References as the decoder sees them, and the slices' lists.
        let references: Vec<H264Reference> = self
            .dpb
            .iter()
            .filter(|frame| frame.marking != Marking::Unused)
            .map(|frame| H264Reference {
                slot: frame.slot,
                long_term: matches!(frame.marking, Marking::LongTerm(_)),
                frame_num_or_long_term_idx: match frame.marking {
                    Marking::LongTerm(index) => index,
                    _ => frame.frame_num,
                },
                top_poc: frame.top_poc,
                bottom_poc: frame.bottom_poc,
            })
            .collect();
        if references.len() > 16 {
            return Err(BitstreamError::Invalid("more than 16 reference frames"));
        }
        let mut picture_slices = Vec::with_capacity(slices.len());
        let mut intra_only = true;
        let current_poc = top_poc.min(bottom_poc);
        for (range, header) in slices {
            intra_only &= header.slice_type == SliceType::I;
            let (ref_list0, ref_list1) = Self::ref_lists(sps, &header, &references, current_poc)?;
            picture_slices.push(H264Slice {
                nal: range,
                header,
                ref_list0,
                ref_list1,
            });
        }

        // Room for the current picture: output waiting pictures if the
        // buffer of references plus waiting pictures is full.
        let capacity = usize::from(stream_format(sps).dpb_slots);
        while self.dpb.len() >= capacity {
            if !self.bump(&mut output) {
                return Err(BitstreamError::Invalid("decoded picture buffer overflow"));
            }
        }
        let output_slots: Vec<Slot> = output
            .iter()
            .map(|picture: &OutputPicture| picture.slot)
            .collect();
        let slot = self.pool.allocate(
            |slot| self.slot_in_use(slot) || output_slots.contains(&slot),
            held,
        )?;
        let picture = H264Picture {
            sps: Arc::clone(sps),
            pps: Arc::clone(pps),
            slot,
            frame_num: first.frame_num,
            idr,
            reference,
            intra_only,
            top_poc,
            bottom_poc,
            references,
            slices: picture_slices,
        };

        // §8.2.5 reference marking for the decoded picture.
        let mut current_marking = if reference {
            Marking::ShortTerm
        } else {
            Marking::Unused
        };
        let mut stored_top = top_poc;
        let mut stored_bottom = bottom_poc;
        let mut stored_frame_num = first.frame_num;
        if let Some(marking) = &first.marking {
            if idr {
                if marking.long_term_reference_flag {
                    current_marking = Marking::LongTerm(0);
                    self.max_long_term_frame_idx = Some(0);
                } else {
                    self.max_long_term_frame_idx = None;
                }
            } else if let Some(operations) = &marking.adaptive {
                for operation in operations {
                    self.apply_mmco(
                        *operation,
                        first.frame_num,
                        max_frame_num,
                        &mut current_marking,
                    )?;
                }
                if has_mmco5 {
                    // §8.2.1: the picture's POC restarts at zero after MMCO 5.
                    let temp = i64::from(stored_top.min(stored_bottom));
                    stored_top = to_poc(i64::from(stored_top) - temp)?;
                    stored_bottom = to_poc(i64::from(stored_bottom) - temp)?;
                    stored_frame_num = 0;
                }
            } else {
                self.sliding_window(sps, first.frame_num, max_frame_num)?;
            }
            let references = self
                .dpb
                .iter()
                .filter(|frame| frame.marking != Marking::Unused)
                .count();
            if references + 1 > usize::from(sps.max_num_ref_frames.max(1)) {
                return Err(BitstreamError::Invalid(
                    "more references than max_num_ref_frames",
                ));
            }
        }
        if has_mmco5 {
            while self.bump(&mut output) {}
            self.last_output_poc = None;
            sequence_start = output.len();
        }
        self.dpb
            .retain(|frame| frame.marking != Marking::Unused || frame.needed_for_output);
        self.dpb.push(DpbFrame {
            slot,
            frame_num: stored_frame_num,
            top_poc: stored_top,
            bottom_poc: stored_bottom,
            marking: current_marking,
            needed_for_output: true,
        });
        let reorder = usize::from(reorder_depth(sps));
        while self
            .dpb
            .iter()
            .filter(|frame| frame.needed_for_output)
            .count()
            > reorder
        {
            if !self.bump(&mut output) {
                break;
            }
        }
        for picture_out in &output[sequence_start..] {
            if let Some(last) = self.last_output_poc {
                if picture_out.poc < last {
                    return Err(BitstreamError::Unsupported(
                        "H.264 picture reordering without bitstream_restriction",
                    ));
                }
            }
            self.last_output_poc = Some(picture_out.poc);
        }

        // POC and frame_num state for the next picture.
        if reference {
            self.prev_ref_frame_num = if has_mmco5 { 0 } else { first.frame_num };
            if let PocType::Lsb { .. } = sps.poc_type {
                if has_mmco5 {
                    self.previous.ref_msb = 0;
                    self.previous.ref_lsb = stored_top;
                } else {
                    let lsb = i32::try_from(first.pic_order_cnt_lsb).unwrap_or(0);
                    self.previous.ref_msb = top_poc - lsb;
                    self.previous.ref_lsb = lsb;
                }
            }
        }
        self.previous.frame_num_offset = if has_mmco5 { 0 } else { frame_num_offset };
        self.previous.frame_num = if has_mmco5 { 0 } else { first.frame_num };
        self.waiting_for_idr = false;
        Ok(H264AccessUnit {
            format_changed,
            format: self.format,
            picture: Some(picture),
            output,
        })
    }

    /// §8.2.1 for a frame: (`TopFieldOrderCnt`, `BottomFieldOrderCnt`,
    /// `FrameNumOffset`).
    fn picture_order_count(
        &self,
        sps: &Sps,
        header: &SliceHeader,
        reference: bool,
        idr: bool,
    ) -> Result<(i32, i32, i64), BitstreamError> {
        let max_frame_num = i64::from(sps.max_frame_num());
        let frame_num = i64::from(header.frame_num);
        let frame_num_offset = if idr {
            0
        } else if i64::from(self.previous.frame_num) > frame_num {
            self.previous
                .frame_num_offset
                .checked_add(max_frame_num)
                .ok_or(BitstreamError::Invalid("FrameNumOffset"))?
        } else {
            self.previous.frame_num_offset
        };
        match &sps.poc_type {
            PocType::Lsb { log2_max_lsb } => {
                let max_lsb = 1_i32 << log2_max_lsb;
                let (prev_msb, prev_lsb) = if idr {
                    (0, 0)
                } else {
                    (self.previous.ref_msb, self.previous.ref_lsb)
                };
                let lsb = i32::try_from(header.pic_order_cnt_lsb).unwrap_or(0);
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    i64::from(prev_msb) + i64::from(max_lsb)
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    i64::from(prev_msb) - i64::from(max_lsb)
                } else {
                    i64::from(prev_msb)
                };
                let top = msb + i64::from(lsb);
                Ok((
                    to_poc(top)?,
                    to_poc(top + i64::from(header.delta_pic_order_cnt_bottom))?,
                    frame_num_offset,
                ))
            }
            PocType::Cycle {
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                offset_for_ref_frame,
                ..
            } => {
                let cycle_length = i64::try_from(offset_for_ref_frame.len()).unwrap_or(0);
                let mut abs_frame_num = if cycle_length == 0 {
                    0
                } else {
                    frame_num_offset
                        .checked_add(frame_num)
                        .ok_or(BitstreamError::Invalid("FrameNumOffset"))?
                };
                if !reference && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected = 0_i64;
                if abs_frame_num > 0 {
                    let delta_per_cycle: i64 =
                        offset_for_ref_frame.iter().map(|v| i64::from(*v)).sum();
                    let cycle_count = (abs_frame_num - 1) / cycle_length;
                    let in_cycle = usize::try_from((abs_frame_num - 1) % cycle_length).unwrap_or(0);
                    expected = cycle_count
                        .checked_mul(delta_per_cycle)
                        .ok_or(BitstreamError::Invalid("expectedPicOrderCnt"))?
                        + offset_for_ref_frame[..=in_cycle]
                            .iter()
                            .map(|v| i64::from(*v))
                            .sum::<i64>();
                }
                if !reference {
                    expected += i64::from(*offset_for_non_ref_pic);
                }
                let top = expected + i64::from(header.delta_pic_order_cnt[0]);
                let bottom = top
                    + i64::from(*offset_for_top_to_bottom_field)
                    + i64::from(header.delta_pic_order_cnt[1]);
                Ok((to_poc(top)?, to_poc(bottom)?, frame_num_offset))
            }
            PocType::DecodingOrder => {
                let doubled = frame_num_offset
                    .checked_add(frame_num)
                    .and_then(|sum| sum.checked_mul(2))
                    .ok_or(BitstreamError::Invalid("FrameNumOffset"))?;
                let temp = if idr {
                    0
                } else if reference {
                    doubled
                } else {
                    doubled - 1
                };
                Ok((to_poc(temp)?, to_poc(temp)?, frame_num_offset))
            }
        }
    }

    /// `PicNum` of a short-term frame (§8.2.4.1, frames).
    fn pic_num(frame_num: u32, current_frame_num: u32, max_frame_num: u32) -> i64 {
        if frame_num > current_frame_num {
            i64::from(frame_num) - i64::from(max_frame_num)
        } else {
            i64::from(frame_num)
        }
    }

    /// §8.2.4.2 initial lists and §8.2.4.3 modification, as indices into
    /// `references`.
    #[allow(clippy::too_many_lines)]
    fn ref_lists(
        sps: &Sps,
        header: &SliceHeader,
        references: &[H264Reference],
        current_poc: i32,
    ) -> Result<RefLists, BitstreamError> {
        if header.slice_type == SliceType::I {
            return Ok((Vec::new(), Vec::new()));
        }
        let max_frame_num = sps.max_frame_num();
        let pic_num = |reference: &H264Reference| {
            Self::pic_num(
                reference.frame_num_or_long_term_idx,
                header.frame_num,
                max_frame_num,
            )
        };
        let poc = |reference: &H264Reference| reference.top_poc.min(reference.bottom_poc);
        let mut short: Vec<usize> = (0..references.len())
            .filter(|i| !references[*i].long_term)
            .collect();
        let mut long: Vec<usize> = (0..references.len())
            .filter(|i| references[*i].long_term)
            .collect();
        long.sort_by_key(|i| references[*i].frame_num_or_long_term_idx);
        let (initial0, initial1) = if header.slice_type == SliceType::P {
            short.sort_by_key(|i| std::cmp::Reverse(pic_num(&references[*i])));
            (
                short.iter().chain(&long).copied().collect::<Vec<_>>(),
                Vec::new(),
            )
        } else {
            let mut before: Vec<usize> = short
                .iter()
                .copied()
                .filter(|i| poc(&references[*i]) < current_poc)
                .collect();
            let mut after: Vec<usize> = short
                .iter()
                .copied()
                .filter(|i| poc(&references[*i]) > current_poc)
                .collect();
            before.sort_by_key(|i| std::cmp::Reverse(poc(&references[*i])));
            after.sort_by_key(|i| poc(&references[*i]));
            let list0: Vec<usize> = before.iter().chain(&after).chain(&long).copied().collect();
            let mut list1: Vec<usize> = after.iter().chain(&before).chain(&long).copied().collect();
            if list1.len() > 1 && list1 == list0 {
                list1.swap(0, 1);
            }
            (list0, list1)
        };
        let modify = |initial: Vec<usize>,
                      active_minus1: u8,
                      operations: Option<&Vec<ListModification>>| {
            let length = usize::from(active_minus1) + 1;
            let mut list: Vec<Option<usize>> = initial.into_iter().map(Some).collect();
            list.resize(length.max(list.len()), None);
            list.truncate(length);
            let Some(operations) = operations else {
                return Ok(list);
            };
            let max_pic_num = i64::from(max_frame_num);
            let current = i64::from(header.frame_num);
            let mut pred = current;
            for (index, operation) in operations.iter().enumerate() {
                let target = match operation {
                    ListModification::Subtract(delta) | ListModification::Add(delta) => {
                        let delta = i64::from(*delta);
                        let mut no_wrap = if matches!(operation, ListModification::Subtract(_)) {
                            pred - delta
                        } else {
                            pred + delta
                        };
                        if no_wrap < 0 {
                            no_wrap += max_pic_num;
                        } else if no_wrap >= max_pic_num {
                            no_wrap -= max_pic_num;
                        }
                        pred = no_wrap;
                        let wanted = if no_wrap > current {
                            no_wrap - max_pic_num
                        } else {
                            no_wrap
                        };
                        short
                            .iter()
                            .copied()
                            .find(|i| pic_num(&references[*i]) == wanted)
                            .ok_or(BitstreamError::MissingReference)?
                    }
                    ListModification::LongTerm(number) => long
                        .iter()
                        .copied()
                        .find(|i| references[*i].frame_num_or_long_term_idx == *number)
                        .ok_or(BitstreamError::MissingReference)?,
                };
                // (8-37)/(8-38): insert at `index`, drop the later duplicate.
                list.insert(index, Some(target));
                let mut kept = Vec::with_capacity(list.len());
                for (position, entry) in list.into_iter().enumerate() {
                    if position <= index || entry != Some(target) {
                        kept.push(entry);
                    }
                }
                list = kept;
                list.truncate(length);
            }
            Ok(list)
        };
        let to_indices = |list: Vec<Option<usize>>| -> Vec<Option<u8>> {
            list.into_iter()
                .map(|entry| entry.and_then(|index| u8::try_from(index).ok()))
                .collect()
        };
        let list0 = modify(
            initial0,
            header.num_ref_idx_l0_active_minus1,
            header.modification_l0.as_ref(),
        )?;
        let list1 = if header.slice_type == SliceType::B {
            modify(
                initial1,
                header.num_ref_idx_l1_active_minus1,
                header.modification_l1.as_ref(),
            )?
        } else {
            Vec::new()
        };
        if list0.first().is_none_or(Option::is_none) {
            // An inter slice needs at least its first reference.
            return Err(BitstreamError::MissingReference);
        }
        Ok((to_indices(list0), to_indices(list1)))
    }

    /// §8.2.5.3 sliding window.
    fn sliding_window(
        &mut self,
        sps: &Sps,
        current_frame_num: u32,
        max_frame_num: u32,
    ) -> Result<(), BitstreamError> {
        let max = usize::from(sps.max_num_ref_frames.max(1));
        let count = self
            .dpb
            .iter()
            .filter(|frame| frame.marking != Marking::Unused)
            .count();
        if count < max {
            return Ok(());
        }
        let oldest = self
            .dpb
            .iter_mut()
            .filter(|frame| frame.marking == Marking::ShortTerm)
            .min_by_key(|frame| Self::pic_num(frame.frame_num, current_frame_num, max_frame_num))
            .ok_or(BitstreamError::Invalid(
                "sliding window without short-term references",
            ))?;
        oldest.marking = Marking::Unused;
        Ok(())
    }

    /// §8.2.5.4 one adaptive marking operation.
    fn apply_mmco(
        &mut self,
        operation: Mmco,
        current_frame_num: u32,
        max_frame_num: u32,
        current: &mut Marking,
    ) -> Result<(), BitstreamError> {
        let short_term =
            |dpb: &mut Vec<DpbFrame>, difference: u32| -> Result<usize, BitstreamError> {
                let target = i64::from(current_frame_num) - (i64::from(difference) + 1);
                dpb.iter()
                    .position(|frame| {
                        frame.marking == Marking::ShortTerm
                            && Self::pic_num(frame.frame_num, current_frame_num, max_frame_num)
                                == target
                    })
                    .ok_or(BitstreamError::MissingReference)
            };
        match operation {
            Mmco::UnmarkShortTerm {
                difference_of_pic_nums_minus1,
            } => {
                let index = short_term(&mut self.dpb, difference_of_pic_nums_minus1)?;
                self.dpb[index].marking = Marking::Unused;
            }
            Mmco::UnmarkLongTerm { long_term_pic_num } => {
                let frame = self
                    .dpb
                    .iter_mut()
                    .find(|frame| frame.marking == Marking::LongTerm(long_term_pic_num))
                    .ok_or(BitstreamError::MissingReference)?;
                frame.marking = Marking::Unused;
            }
            Mmco::ShortToLong {
                difference_of_pic_nums_minus1,
                long_term_frame_idx,
            } => {
                if self
                    .max_long_term_frame_idx
                    .is_none_or(|max| long_term_frame_idx > max)
                {
                    return Err(BitstreamError::Invalid("long_term_frame_idx"));
                }
                let index = short_term(&mut self.dpb, difference_of_pic_nums_minus1)?;
                for frame in &mut self.dpb {
                    if frame.marking == Marking::LongTerm(long_term_frame_idx) {
                        frame.marking = Marking::Unused;
                    }
                }
                self.dpb[index].marking = Marking::LongTerm(long_term_frame_idx);
            }
            Mmco::MaxLongTermIndex {
                max_long_term_frame_idx_plus1,
            } => {
                self.max_long_term_frame_idx = max_long_term_frame_idx_plus1.checked_sub(1);
                for frame in &mut self.dpb {
                    if let Marking::LongTerm(index) = frame.marking {
                        if self.max_long_term_frame_idx.is_none_or(|max| index > max) {
                            frame.marking = Marking::Unused;
                        }
                    }
                }
            }
            Mmco::UnmarkAll => {
                for frame in &mut self.dpb {
                    frame.marking = Marking::Unused;
                }
                self.max_long_term_frame_idx = None;
            }
            Mmco::CurrentToLong {
                long_term_frame_idx,
            } => {
                if self
                    .max_long_term_frame_idx
                    .is_none_or(|max| long_term_frame_idx > max)
                {
                    return Err(BitstreamError::Invalid("long_term_frame_idx"));
                }
                for frame in &mut self.dpb {
                    if frame.marking == Marking::LongTerm(long_term_frame_idx) {
                        frame.marking = Marking::Unused;
                    }
                }
                *current = Marking::LongTerm(long_term_frame_idx);
            }
        }
        Ok(())
    }

    /// Outputs the waiting frame with the smallest POC.
    fn bump(&mut self, output: &mut Vec<OutputPicture>) -> bool {
        let Some(index) = self
            .dpb
            .iter()
            .enumerate()
            .filter(|(_, frame)| frame.needed_for_output)
            .min_by_key(|(_, frame)| frame.poc())
            .map(|(index, _)| index)
        else {
            return false;
        };
        let frame = &mut self.dpb[index];
        frame.needed_for_output = false;
        output.push(OutputPicture {
            slot: frame.slot,
            poc: frame.poc(),
        });
        if frame.marking == Marking::Unused {
            self.dpb.remove(index);
        }
        true
    }
}

/// `RefPicList0` and `RefPicList1` of one slice.
type RefLists = (Vec<Option<u8>>, Vec<Option<u8>>);

/// A picture order count that fits the `i32` every API uses.
fn to_poc(value: i64) -> Result<i32, BitstreamError> {
    i32::try_from(value).map_err(|_| BitstreamError::Invalid("PicOrderCnt"))
}

/// Pictures that may wait for output: the declared reorder depth, else none
/// (see the module documentation).
fn reorder_depth(sps: &Sps) -> u8 {
    sps.vui
        .and_then(|vui| vui.max_num_reorder_frames)
        .unwrap_or(0)
        .min(16)
}

/// The decoder-session facts of an SPS.
#[must_use]
pub fn stream_format(sps: &Sps) -> StreamFormat {
    let width = sps.pic_width_in_mbs * 16;
    let height = sps.frame_height_in_mbs() * 16;
    let (left, right, top, bottom) = sps.crop;
    StreamFormat {
        codec: StreamCodecKind::H264,
        profile_idc: sps.profile_idc,
        coded_width: width,
        coded_height: height,
        visible: (left, top, width - left - right, height - top - bottom),
        chroma_format_idc: sps.chroma_format_idc,
        bit_depth_luma: sps.bit_depth_luma,
        bit_depth_chroma: sps.bit_depth_chroma,
        dpb_slots: sps.max_num_ref_frames.max(1) + reorder_depth(sps) + 1,
        colour: sps.vui.and_then(|vui| vui.colour),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sps(max_num_ref_frames: u8) -> Sps {
        let mut sps = Sps::parse(&crate::bitstream::unescape_rbsp(
            &crate::bitstream::synth::build_h264_sps()[1..],
        ))
        .unwrap();
        sps.max_num_ref_frames = max_num_ref_frames;
        sps
    }

    fn frame(slot: u8, frame_num: u32, marking: Marking) -> DpbFrame {
        DpbFrame {
            slot: Slot(slot),
            frame_num,
            top_poc: i32::try_from(frame_num).unwrap() * 2,
            bottom_poc: i32::try_from(frame_num).unwrap() * 2,
            marking,
            needed_for_output: false,
        }
    }

    #[test]
    fn the_sliding_window_drops_the_oldest_short_term_frame_across_a_wrap() {
        let mut parser = H264Parser::new(0);
        // MaxFrameNum 16: frame 14 is older than frame 1 when decoding 2.
        parser.dpb = vec![
            frame(0, 14, Marking::ShortTerm),
            frame(1, 1, Marking::ShortTerm),
        ];
        parser.sliding_window(&sps(2), 2, 16).unwrap();
        assert_eq!(parser.dpb[0].marking, Marking::Unused);
        assert_eq!(parser.dpb[1].marking, Marking::ShortTerm);
    }

    #[test]
    fn mmco_operations_mark_as_specified() {
        let mut parser = H264Parser::new(0);
        parser.dpb = vec![
            frame(0, 3, Marking::ShortTerm),
            frame(1, 4, Marking::ShortTerm),
            frame(2, 1, Marking::LongTerm(0)),
        ];
        let mut current = Marking::ShortTerm;
        parser
            .apply_mmco(
                Mmco::MaxLongTermIndex {
                    max_long_term_frame_idx_plus1: 2,
                },
                5,
                16,
                &mut current,
            )
            .unwrap();
        // 3: picNumX = 5 - 2 = 3 becomes long-term index 0, replacing frame 1.
        parser
            .apply_mmco(
                Mmco::ShortToLong {
                    difference_of_pic_nums_minus1: 1,
                    long_term_frame_idx: 0,
                },
                5,
                16,
                &mut current,
            )
            .unwrap();
        assert_eq!(parser.dpb[0].marking, Marking::LongTerm(0));
        assert_eq!(parser.dpb[2].marking, Marking::Unused);
        // 1: picNumX = 5 - 1 = 4 becomes unused.
        parser
            .apply_mmco(
                Mmco::UnmarkShortTerm {
                    difference_of_pic_nums_minus1: 0,
                },
                5,
                16,
                &mut current,
            )
            .unwrap();
        assert_eq!(parser.dpb[1].marking, Marking::Unused);
        parser
            .apply_mmco(
                Mmco::CurrentToLong {
                    long_term_frame_idx: 1,
                },
                5,
                16,
                &mut current,
            )
            .unwrap();
        assert_eq!(current, Marking::LongTerm(1));
        // A picture that is not there is a missing reference, not a guess.
        assert_eq!(
            parser.apply_mmco(
                Mmco::UnmarkShortTerm {
                    difference_of_pic_nums_minus1: 7
                },
                5,
                16,
                &mut current
            ),
            Err(BitstreamError::MissingReference)
        );
        parser
            .apply_mmco(Mmco::UnmarkAll, 5, 16, &mut current)
            .unwrap();
        assert!(
            parser
                .dpb
                .iter()
                .all(|frame| frame.marking == Marking::Unused)
        );
        assert_eq!(parser.max_long_term_frame_idx, None);
    }

    fn p_header(
        frame_num: u32,
        active_minus1: u8,
        modification: Option<Vec<ListModification>>,
    ) -> SliceHeader {
        SliceHeader {
            first_mb_in_slice: 0,
            slice_type: SliceType::P,
            pps_id: 0,
            colour_plane_id: 0,
            frame_num,
            idr_pic_id: 0,
            pic_order_cnt_lsb: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0; 2],
            redundant_pic_cnt: 0,
            direct_spatial_mv_pred_flag: false,
            num_ref_idx_l0_active_minus1: active_minus1,
            num_ref_idx_l1_active_minus1: 0,
            modification_l0: modification,
            modification_l1: None,
            marking: None,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
        }
    }

    fn reference(slot: u8, number: u32, long_term: bool) -> H264Reference {
        H264Reference {
            slot: Slot(slot),
            long_term,
            frame_num_or_long_term_idx: number,
            top_poc: 0,
            bottom_poc: 0,
        }
    }

    #[test]
    fn p_lists_order_short_term_by_pic_num_then_long_term_and_apply_modifications() {
        let sps = sps(4);
        let references = [
            reference(0, 2, false),
            reference(1, 5, true),
            reference(2, 4, false),
            reference(3, 0, true),
        ];
        let (list0, _) =
            H264Parser::ref_lists(&sps, &p_header(5, 3, None), &references, 10).unwrap();
        assert_eq!(list0, vec![Some(2), Some(0), Some(3), Some(1)]);
        // Long-term index 5 first, then picNum 5 - 3 = 2.
        let modification = vec![ListModification::LongTerm(5), ListModification::Subtract(3)];
        let (list0, _) =
            H264Parser::ref_lists(&sps, &p_header(5, 3, Some(modification)), &references, 10)
                .unwrap();
        assert_eq!(list0, vec![Some(1), Some(0), Some(2), Some(3)]);
        let missing = vec![ListModification::Subtract(2)];
        assert_eq!(
            H264Parser::ref_lists(&sps, &p_header(5, 0, Some(missing)), &references, 10),
            Err(BitstreamError::MissingReference)
        );
    }

    #[test]
    fn picture_order_count_type_2_doubles_frame_num_across_wraps() {
        let mut parser = H264Parser::new(0);
        let mut sps = sps(1);
        sps.poc_type = PocType::DecodingOrder;
        let mut header = p_header(15, 0, None);
        parser.previous.frame_num = 14;
        let (top, _, offset) = parser
            .picture_order_count(&sps, &header, true, false)
            .unwrap();
        assert_eq!((top, offset), (30, 0));
        parser.previous.frame_num = 15;
        header.frame_num = 0;
        let (top, _, offset) = parser
            .picture_order_count(&sps, &header, true, false)
            .unwrap();
        assert_eq!((top, offset), (32, 16));
    }
}
