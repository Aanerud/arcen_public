// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Decoder-independent picture model shared by the H.264 and H.265 parsers.
//!
//! A hardware decoder (DXVA, VA-API, Vulkan Video) does the reconstruction;
//! the parser decides *which* picture goes where: the surface ("slot") the
//! current picture decodes into, the reference pictures it may read, and
//! which decoded pictures are ready to show and in what order. Slots are
//! small integers so an adapter can map them onto a texture array.

use super::BitstreamError;

/// A decoded-picture surface index, `0..pool size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(pub u8);

impl Slot {
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A picture the decoder has finished and the presenter may show now, in
/// output order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputPicture {
    pub slot: Slot,
    /// Picture order count, for diagnostics.
    pub poc: i32,
}

/// The stream facts a hardware decoder session is created from. When this
/// changes the adapter must build a new decoder (and its surfaces).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub codec: StreamCodecKind,
    /// `profile_idc` as coded (H.264 `profile_idc`, H.265
    /// `general_profile_idc`).
    pub profile_idc: u8,
    /// Coded (decoded surface) size in luma samples.
    pub coded_width: u32,
    pub coded_height: u32,
    /// The visible rectangle inside the coded picture (cropping /
    /// conformance window): left, top, width, height.
    pub visible: (u32, u32, u32, u32),
    /// `chroma_format_idc`: 0 monochrome, 1 4:2:0, 2 4:2:2, 3 4:4:4.
    pub chroma_format_idc: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    /// Slots the stream needs: its decoded picture buffer plus the picture
    /// being decoded. An adapter adds the pictures it holds for display.
    pub dpb_slots: u8,
    /// VUI colour description, when stated.
    pub colour: Option<ColourDescription>,
}

/// Which syntax a [`StreamFormat`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamCodecKind {
    H264,
    H265,
}

/// VUI `video_signal_type` fields (ITU-T H.273 code points).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourDescription {
    pub full_range: bool,
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

impl StreamFormat {
    /// What the stream's own syntax contradicts in the contract the wire
    /// claims for it, or `None` when they agree.
    ///
    /// The header is the host's claim; the parameter sets are what the
    /// decoder obeys. A Deck that presented the claim would mislabel the
    /// picture (a ten-bit 4:4:4 Grading contract on an eight-bit 4:2:0
    /// stream), so a contradiction is refused, never presented. Codec,
    /// chroma and depth are always stated by the stream; range, matrix and
    /// primaries only when its VUI describes them (code 2, "unspecified",
    /// states nothing); transfer only as SDR versus PQ versus HLG, because
    /// SDR encoders tag BT.709 and sRGB interchangeably.
    #[must_use]
    pub fn contradiction(&self, claimed: &crate::VideoConfiguration) -> Option<String> {
        let mut found = Vec::new();
        let codec_matches = matches!(
            (self.codec, claimed.codec),
            (StreamCodecKind::H264, crate::VideoCodec::H264)
                | (StreamCodecKind::H265, crate::VideoCodec::H265)
        );
        if !codec_matches {
            found.push(format!(
                "the stream is {:?} but its header says {:?}",
                self.codec, claimed.codec
            ));
        }
        let chroma_idc = match claimed.chroma {
            crate::ChromaSubsampling::Yuv420 => 1,
            crate::ChromaSubsampling::Yuv422 => 2,
            crate::ChromaSubsampling::Yuv444 => 3,
        };
        let depth = claimed.bit_depth.bits();
        if self.chroma_format_idc != chroma_idc
            || u16::from(self.bit_depth_luma) != depth
            || u16::from(self.bit_depth_chroma) != depth
        {
            found.push(format!(
                "the stream is chroma_format_idc {} at {}/{} bits but its header says {:?} {depth}-bit",
                self.chroma_format_idc, self.bit_depth_luma, self.bit_depth_chroma, claimed.chroma
            ));
        }
        if let Some(colour) = self.colour {
            let full = claimed.range == crate::ColorRange::Full;
            if colour.full_range != full {
                found.push(format!(
                    "the stream is {} range but its header says {:?}",
                    if colour.full_range { "full" } else { "limited" },
                    claimed.range
                ));
            }
            let matrix = match colour.matrix {
                2 | 3 => None,
                0 => Some(Some(crate::ColorMatrix::Identity)),
                1 => Some(Some(crate::ColorMatrix::Bt709)),
                5 | 6 => Some(Some(crate::ColorMatrix::Bt601)),
                9 => Some(Some(crate::ColorMatrix::Bt2020Ncl)),
                _ => Some(None),
            };
            if let Some(stated) = matrix
                && stated != Some(claimed.matrix)
            {
                found.push(format!(
                    "the stream's matrix is code {} but its header says {:?}",
                    colour.matrix, claimed.matrix
                ));
            }
            let primaries = match colour.primaries {
                2 | 3 => None,
                1 => Some(Some(crate::ColorPrimaries::Bt709)),
                9 => Some(Some(crate::ColorPrimaries::Bt2020)),
                12 => Some(Some(crate::ColorPrimaries::DisplayP3)),
                _ => Some(None),
            };
            if let Some(stated) = primaries
                && stated != Some(claimed.primaries)
            {
                found.push(format!(
                    "the stream's primaries are code {} but the session says {:?}",
                    colour.primaries, claimed.primaries
                ));
            }
            let class = |transfer: crate::TransferCharacteristics| match transfer {
                crate::TransferCharacteristics::Pq => "PQ",
                crate::TransferCharacteristics::Hlg => "HLG",
                crate::TransferCharacteristics::Bt709 | crate::TransferCharacteristics::Srgb => {
                    "SDR"
                }
            };
            let stated = match colour.transfer {
                2 | 3 => None,
                16 => Some("PQ"),
                18 => Some("HLG"),
                _ => Some("SDR"),
            };
            if let Some(stated) = stated
                && stated != class(claimed.transfer)
            {
                found.push(format!(
                    "the stream's transfer is {stated} (code {}) but the session says {:?}",
                    colour.transfer, claimed.transfer
                ));
            }
        }
        (!found.is_empty()).then(|| found.join("; "))
    }
}

/// The identity of the access unit that decoded each slot (its wire
/// timestamp and contract), so a picture output by a later access unit
/// (reordered, or bumped out of the buffer) is reported as itself, not as
/// the unit that happened to release it.
#[derive(Debug, Clone)]
pub struct SlotTags<T> {
    tags: Vec<Option<T>>,
}

impl<T> Default for SlotTags<T> {
    fn default() -> Self {
        Self { tags: Vec::new() }
    }
}

impl<T> SlotTags<T> {
    /// `slot` now holds the picture of the access unit described by `tag`.
    pub fn decoded(&mut self, slot: Slot, tag: T) {
        let index = slot.index();
        if self.tags.len() <= index {
            self.tags.resize_with(index + 1, || None);
        }
        self.tags[index] = Some(tag);
    }

    /// The identity of the picture `slot` outputs; each decoded picture is
    /// output once, so the tag is taken.
    pub fn output(&mut self, slot: Slot) -> Option<T> {
        self.tags.get_mut(slot.index()).and_then(Option::take)
    }

    /// Forgets every slot (decoder reset).
    pub fn clear(&mut self) {
        self.tags.clear();
    }
}

/// Allocates slots for pictures being decoded: the lowest index that is
/// neither held by the decoded picture buffer nor by the caller (a presenter
/// still showing it).
#[derive(Debug, Clone, Default)]
pub struct SlotPool {
    size: u8,
}

impl SlotPool {
    #[must_use]
    pub const fn new(size: u8) -> Self {
        Self { size }
    }

    #[must_use]
    pub const fn size(&self) -> u8 {
        self.size
    }

    /// The lowest free slot.
    ///
    /// # Errors
    ///
    /// [`BitstreamError::NoFreeSlot`] when every slot is in use.
    pub fn allocate(
        &self,
        in_dpb: impl Fn(Slot) -> bool,
        held: &dyn Fn(Slot) -> bool,
    ) -> Result<Slot, BitstreamError> {
        (0..self.size)
            .map(Slot)
            .find(|slot| !in_dpb(*slot) && !held(*slot))
            .ok_or(BitstreamError::NoFreeSlot)
    }
}

/// `Ceil(Log2(value))` for the bit widths of `u(v)` fields (0 for 0 or 1).
#[must_use]
pub(crate) const fn ceil_log2(value: u32) -> u32 {
    if value <= 1 {
        0
    } else {
        32 - (value - 1).leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_log2_matches_the_specification_helper() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(510), 9);
    }

    fn grading_format() -> StreamFormat {
        StreamFormat {
            codec: StreamCodecKind::H265,
            profile_idc: 4,
            coded_width: 640,
            coded_height: 368,
            visible: (0, 0, 640, 360),
            chroma_format_idc: 3,
            bit_depth_luma: 10,
            bit_depth_chroma: 10,
            dpb_slots: 2,
            colour: Some(ColourDescription {
                full_range: true,
                primaries: 1,
                transfer: 13,
                matrix: 1,
            }),
        }
    }

    fn grading_claim() -> crate::VideoConfiguration {
        crate::VideoConfiguration {
            codec: crate::VideoCodec::H265,
            chroma: crate::ChromaSubsampling::Yuv444,
            bit_depth: crate::BitDepth::Ten,
            range: crate::ColorRange::Full,
            matrix: crate::ColorMatrix::Bt709,
            primaries: crate::ColorPrimaries::Bt709,
            transfer: crate::TransferCharacteristics::Srgb,
        }
    }

    #[test]
    fn a_stream_that_matches_its_claim_has_no_contradiction() {
        assert_eq!(grading_format().contradiction(&grading_claim()), None);
        // SDR encoders tag BT.709 or sRGB transfer for the same contract.
        let mut bt709_tagged = grading_format();
        bt709_tagged.colour = bt709_tagged.colour.map(|colour| ColourDescription {
            transfer: 1,
            ..colour
        });
        assert_eq!(bt709_tagged.contradiction(&grading_claim()), None);
        // "Unspecified" VUI codes (the macOS Pier's Auto) state nothing.
        let mut unspecified = grading_format();
        unspecified.colour = Some(ColourDescription {
            full_range: true,
            primaries: 2,
            transfer: 2,
            matrix: 2,
        });
        assert_eq!(unspecified.contradiction(&grading_claim()), None);
        let mut undescribed = grading_format();
        undescribed.colour = None;
        assert_eq!(undescribed.contradiction(&grading_claim()), None);
    }

    #[test]
    fn a_grading_claim_on_an_eight_bit_4_2_0_stream_is_a_contradiction() {
        let mut main = grading_format();
        main.chroma_format_idc = 1;
        main.bit_depth_luma = 8;
        main.bit_depth_chroma = 8;
        let reason = main.contradiction(&grading_claim()).unwrap();
        assert!(
            reason.contains("chroma_format_idc 1 at 8/8 bits"),
            "{reason}"
        );
    }

    #[test]
    fn range_matrix_primaries_hdr_and_codec_contradictions_are_named() {
        let claim = grading_claim();
        let check = |colour: ColourDescription, needle: &str| {
            let mut format = grading_format();
            format.colour = Some(colour);
            let reason = format.contradiction(&claim).unwrap();
            assert!(reason.contains(needle), "{reason}");
        };
        let base = grading_format().colour.unwrap();
        check(
            ColourDescription {
                full_range: false,
                ..base
            },
            "limited range",
        );
        check(ColourDescription { matrix: 9, ..base }, "matrix is code 9");
        check(
            ColourDescription { matrix: 10, ..base },
            "matrix is code 10",
        );
        check(
            ColourDescription {
                primaries: 9,
                ..base
            },
            "primaries are code 9",
        );
        check(
            ColourDescription {
                transfer: 16,
                ..base
            },
            "transfer is PQ",
        );
        let mut hdr_claim = claim;
        hdr_claim.transfer = crate::TransferCharacteristics::Pq;
        assert!(
            grading_format()
                .contradiction(&hdr_claim)
                .unwrap()
                .contains("transfer is SDR")
        );
        let mut h264 = grading_format();
        h264.codec = StreamCodecKind::H264;
        assert!(h264.contradiction(&claim).unwrap().contains("H264"));
    }

    #[test]
    fn slot_tags_follow_the_picture_not_the_unit_that_outputs_it() {
        let mut tags = SlotTags::default();
        // Unit 10 decodes into slot 0 and is held for reordering; unit 20
        // decodes into slot 1 and releases slot 0.
        tags.decoded(Slot(0), 10_u32);
        tags.decoded(Slot(1), 20_u32);
        assert_eq!(tags.output(Slot(0)), Some(10));
        assert_eq!(tags.output(Slot(0)), None, "each picture is output once");
        tags.decoded(Slot(0), 30);
        assert_eq!(tags.output(Slot(1)), Some(20));
        assert_eq!(tags.output(Slot(0)), Some(30));
        assert_eq!(tags.output(Slot(7)), None);
        tags.decoded(Slot(2), 40);
        tags.clear();
        assert_eq!(tags.output(Slot(2)), None);
    }

    #[test]
    fn slot_pool_skips_dpb_and_held_slots() {
        let pool = SlotPool::new(3);
        let slot = pool
            .allocate(|slot| slot == Slot(0), &|slot| slot == Slot(1))
            .unwrap();
        assert_eq!(slot, Slot(2));
        assert_eq!(
            pool.allocate(|_| true, &|_| false),
            Err(BitstreamError::NoFreeSlot)
        );
    }
}
