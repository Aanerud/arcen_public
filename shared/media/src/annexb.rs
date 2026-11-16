// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Reading and rewriting H.264 and H.265 access units.
//!
//! Two encoders in this repository produce *length-prefixed* samples — macOS
//! `VideoToolbox` and Windows Media Foundation both emit four-byte lengths —
//! while the wire format, and Linux's capture path, use Annex B start codes.
//! Converting between them and deciding whether a unit is a keyframe is byte
//! arithmetic over a documented bitstream layout. None of it touches an
//! operating system, and getting it wrong is not obvious from the outside: a
//! stream whose keyframes are misreported still plays, right up until a client
//! reconnects and has nothing to start from.
//!
//! There is one trap here worth stating plainly, because it was a real bug.
//! The two codecs' NAL headers overlap. An H.264 P-slice header of `0x21`
//! reads as HEVC `nal_unit_type` 16, which is inside the IRAP range — so code
//! that tries both layouts marks every H.264 frame a keyframe. Classification
//! must follow the codec the session actually negotiated, which is why every
//! function here takes one.

/// The codec whose NAL layout applies.
///
/// Not defaulted, and not inferred from the bytes: the two layouts alias, so
/// guessing produces a confident wrong answer rather than an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    H265,
}

/// The four-byte Annex B start code.
///
/// Three-byte start codes are accepted when scanning, but only the four-byte
/// form is written: it is what every decoder in this repository's test matrix
/// has been exercised against.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Returns whether a single NAL unit begins an intra refresh.
///
/// `nal` must start at the NAL header byte, with no start code or length
/// prefix.
///
/// The header is validated rather than just masked. A byte with the forbidden
/// zero bit set, a reserved type, or an impossible temporal id is not a NAL
/// unit this code should be drawing conclusions from — and the alternative,
/// masking whatever arrives and trusting the result, is how a corrupt byte
/// becomes a confidently reported keyframe.
///
/// These are the rules the Linux host already applied. The shared contract
/// takes the stricter of the implementations it replaces, so migrating a
/// platform onto it is never a downgrade.
#[must_use]
pub fn nal_is_irap(nal: &[u8], codec: NalCodec) -> bool {
    match codec {
        NalCodec::H264 => {
            let Some((&header, body)) = nal.split_first() else {
                return false;
            };
            // forbidden_zero_bit must be clear, and a slice has a payload.
            if header & 0x80 != 0 || body.is_empty() {
                return false;
            }
            // An IDR slice is always referenced; nal_ref_idc of zero means
            // the encoder marked it disposable, which an IDR cannot be.
            let nal_ref_idc = (header >> 5) & 0x03;
            header & 0x1F == 5 && nal_ref_idc != 0
        }
        NalCodec::H265 => {
            // Two-byte header plus at least one payload byte.
            if nal.len() < 3 || !nal_header_is_valid(nal, codec) {
                return false;
            }
            let nal_type = (nal[0] >> 1) & 0x3F;
            let temporal_id_plus_one = nal[1] & 0x07;
            // 16..=21 are the IRAP types a real encoder emits (H.265 §7.4.2.2,
            // Table 7-1): BLA_W_LP, BLA_W_RADL, BLA_N_LP, IDR_W_RADL, IDR_N_LP
            // and CRA_NUT. Decoding may start at any of them; the leading
            // pictures a CRA or BLA cannot reconstruct (RASL) are skipped by
            // the decoder, not shown wrong. 22 and 23 are reserved, and
            // treating a reserved type as a keyframe would tell a client to
            // start decoding at a picture whose meaning is not defined. An
            // IRAP is always at the base temporal layer, and only a base
            // layer (nuh_layer_id 0) picture is one a single-layer decoder
            // starts from: it discards every other layer.
            (16..=21).contains(&nal_type)
                && temporal_id_plus_one == 1
                && h265_nuh_layer_id(nal) == 0
        }
    }
}

/// `nuh_layer_id`, which straddles the two H.265 header bytes.
const fn h265_nuh_layer_id(nal: &[u8]) -> u8 {
    ((nal[0] & 0x01) << 5) | (nal[1] >> 3)
}

/// Returns whether a NAL unit's header is one this module may draw any
/// conclusion from: long enough for the codec's header, the forbidden zero
/// bit clear, and a type the codec defines for an Annex B byte stream.
///
/// H.264 types 0 and 24..=31 are unspecified (24..=29 are RTP aggregation
/// and fragmentation units, which never appear in a byte stream); H.265
/// types 48..=63 are unspecified for the same reason, and a
/// `nuh_temporal_id_plus1` of zero is forbidden.
#[must_use]
pub fn nal_header_is_valid(nal: &[u8], codec: NalCodec) -> bool {
    match codec {
        NalCodec::H264 => nal
            .first()
            .is_some_and(|header| header & 0x80 == 0 && !matches!(header & 0x1F, 0 | 24..=31)),
        NalCodec::H265 => {
            nal.len() >= 2 && nal[0] & 0x80 == 0 && nal[1] & 0x07 != 0 && (nal[0] >> 1) & 0x3F <= 47
        }
    }
}

/// Returns whether a single NAL unit is a parameter set.
///
/// Parameter sets have to precede the keyframe they describe, or a client that
/// joins mid-stream cannot decode the picture it was given.
#[must_use]
pub fn nal_is_parameter_set(nal: &[u8], codec: NalCodec) -> bool {
    let Some(&header) = nal.first() else {
        return false;
    };
    match codec {
        // 7 is SPS, 8 is PPS. Both are referenced by definition.
        NalCodec::H264 => {
            header & 0x80 == 0 && matches!(header & 0x1F, 7 | 8) && (header >> 5) & 0x03 != 0
        }
        // 32 is VPS, 33 is SPS, 34 is PPS.
        NalCodec::H265 => header & 0x80 == 0 && matches!((header >> 1) & 0x3F, 32..=34),
    }
}

/// Why a length-prefixed (AVCC/HVCC) sample could not be walked to its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthPrefixedError {
    /// Fewer than four bytes were left for the next length.
    TruncatedLength,
    /// A length claimed more bytes than the sample holds.
    LengthPastEnd,
}

impl std::fmt::Display for LengthPrefixedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TruncatedLength => "length-prefixed sample ended mid-length-prefix",
            Self::LengthPastEnd => "length prefix exceeds the remaining sample",
        })
    }
}

impl std::error::Error for LengthPrefixedError {}

/// Walks the NAL units of a length-prefixed sample (four-byte big-endian
/// lengths, as `VideoToolbox` and Media Foundation write them).
///
/// A malformed length ends the walk with an error rather than a guess,
/// because a sample that does not describe itself is not one to interpret
/// further; the units before it have already been visited. A zero length is
/// skipped: it is not a NAL unit, but it is self-describing (four bytes, no
/// payload), and some Media Foundation transforms emit one as an empty tail.
fn walk_length_prefixed<'a>(
    sample: &'a [u8],
    mut visit: impl FnMut(&'a [u8]),
) -> Result<(), LengthPrefixedError> {
    let mut rest = sample;
    while !rest.is_empty() {
        let Some((length, tail)) = rest.split_first_chunk::<4>() else {
            return Err(LengthPrefixedError::TruncatedLength);
        };
        let Ok(length) = usize::try_from(u32::from_be_bytes(*length)) else {
            return Err(LengthPrefixedError::LengthPastEnd);
        };
        if length > tail.len() {
            return Err(LengthPrefixedError::LengthPastEnd);
        }
        let (nal, next) = tail.split_at(length);
        if !nal.is_empty() {
            visit(nal);
        }
        rest = next;
    }
    Ok(())
}

/// Rewrites a length-prefixed sample into Annex B, appending to `out`.
///
/// Infallible: a malformed tail is dropped and the units before it are kept.
/// Use [`length_prefixed_to_annex_b`] where a malformed sample must be
/// refused instead.
pub fn append_length_prefixed_as_annex_b(out: &mut Vec<u8>, sample: &[u8]) {
    let _ = walk_length_prefixed(sample, |nal| {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    });
}

/// Rewrites a length-prefixed sample into Annex B, appending to `out`, or
/// refuses it whole.
///
/// # Errors
///
/// [`LengthPrefixedError`] for a truncated length or a length past the end of
/// the sample; `out` is then left as it was.
pub fn length_prefixed_to_annex_b(
    sample: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), LengthPrefixedError> {
    let original = out.len();
    let walked = walk_length_prefixed(sample, |nal| {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    });
    if walked.is_err() {
        out.truncate(original);
    }
    walked
}

/// Returns whether a length-prefixed sample contains an IRAP/IDR unit.
#[must_use]
pub fn length_prefixed_is_keyframe(sample: &[u8], codec: NalCodec) -> bool {
    let mut found = false;
    let _ = walk_length_prefixed(sample, |nal| {
        if !found && nal_is_irap(nal, codec) {
            found = true;
        }
    });
    found
}

/// Iterator over the NAL units of an Annex B buffer; see [`nal_units`].
#[derive(Debug, Clone)]
pub struct NalUnits<'a> {
    data: &'a [u8],
    /// Start of the next unit's payload, just past its start code.
    next: Option<usize>,
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let start = self.next?;
            let (end, following) = match find_start_code(self.data, start) {
                Some((index, length)) => (index, Some(index + length)),
                None => (self.data.len(), None),
            };
            self.next = following;
            if let Some(unit) = trim_trailing_zeros(&self.data[start..end]) {
                return Some(unit);
            }
        }
    }
}

/// Walks the NAL units of an Annex B buffer.
///
/// Three- and four-byte start codes are both accepted, because a real
/// bitstream mixes them. Anything before the first start code is not a NAL
/// unit and is skipped. Trailing zero bytes (`trailing_zero_8bits`, or the
/// leading zero of a following four-byte start code) are not part of a unit
/// and are trimmed; a unit that is nothing but zeros is dropped.
#[must_use]
pub fn nal_units(data: &[u8]) -> NalUnits<'_> {
    NalUnits {
        data,
        next: find_start_code(data, 0).map(|(index, length)| index + length),
    }
}

/// Finds the next start code (`00 00 01`, or `00 00 00 01`) at or after
/// `from`, returning its offset and length.
#[must_use]
pub fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

/// Returns whether `data` begins with a start code (`00 00 01` or
/// `00 00 00 01`): how an encoder that may emit either framing is told apart.
#[must_use]
pub fn starts_with_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&START_CODE)
}

fn trim_trailing_zeros(unit: &[u8]) -> Option<&[u8]> {
    let end = unit.iter().rposition(|byte| *byte != 0)? + 1;
    Some(&unit[..end])
}

/// Walks the NAL units of an Annex B access unit; see [`nal_units`].
pub fn for_each_annex_b(access_unit: &[u8], mut visit: impl FnMut(&[u8])) {
    for unit in nal_units(access_unit) {
        visit(unit);
    }
}

/// Splits a whole Annex B buffer (for example a capture file) into NAL units.
///
/// Unlike [`nal_units`], a buffer with no start code at all is one bare NAL
/// unit (a lone raw parameter set), not nothing.
#[must_use]
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    if find_start_code(data, 0).is_none() {
        return (!data.is_empty()).then_some(data).into_iter().collect();
    }
    nal_units(data).collect()
}

/// Splits a recorded Annex B elementary stream into access units (ITU-T
/// H.264 §7.4.1.2.3, H.265 §7.4.2.4.4): a unit ends before an access unit
/// delimiter, a parameter set or prefix SEI that follows a picture, or the
/// first slice of the next picture. Each returned unit keeps its start codes.
/// Used to replay captured streams; wire packets are already one unit each.
#[must_use]
pub fn split_access_units(codec: NalCodec, data: &[u8]) -> Vec<&[u8]> {
    let base = data.as_ptr() as usize;
    let mut units = Vec::new();
    let mut unit_start = 0_usize;
    let mut seen_picture = false;
    for nal in nal_units(data) {
        let offset = nal.as_ptr() as usize - base;
        // Back up over the start code (three bytes plus an optional zero).
        let mut nal_start = offset.saturating_sub(3);
        if nal_start > 0 && data[nal_start - 1] == 0 {
            nal_start -= 1;
        }
        let (starts_unit, is_picture) = match codec {
            NalCodec::H264 => {
                let kind = nal[0] & 0x1f;
                let first_slice =
                    matches!(kind, 1 | 5) && nal.get(1).is_some_and(|b| b & 0x80 != 0);
                (
                    matches!(kind, 6..=9 | 14..=18) || first_slice,
                    matches!(kind, 1 | 5),
                )
            }
            NalCodec::H265 => {
                let kind = (nal[0] >> 1) & 0x3f;
                let first_slice = kind < 32 && nal.get(2).is_some_and(|b| b & 0x80 != 0);
                (
                    matches!(kind, 32..=35 | 39 | 41..=44 | 48..=55) || first_slice,
                    kind < 32,
                )
            }
        };
        if starts_unit && seen_picture && nal_start > unit_start {
            units.push(&data[unit_start..nal_start]);
            unit_start = nal_start;
            seen_picture = false;
        }
        seen_picture |= is_picture;
    }
    if unit_start < data.len() && seen_picture {
        units.push(&data[unit_start..]);
    }
    units
}

/// Why a wire access unit was refused before any decoder saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessUnitError {
    Empty,
    MissingStartCode,
    DataBeforeStartCode,
    NoNalUnits,
    TruncatedHeader { codec: NalCodec, length: usize },
    ForbiddenBit { codec: NalCodec },
}

impl std::fmt::Display for AccessUnitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("empty Annex-B access unit"),
            Self::MissingStartCode => {
                formatter.write_str("access unit is not Annex-B (missing leading start code)")
            }
            Self::DataBeforeStartCode => formatter
                .write_str("Annex-B access unit has non-zero data before its first start code"),
            Self::NoNalUnits => formatter.write_str("Annex-B access unit contains no NAL units"),
            Self::TruncatedHeader { codec, length } => {
                write!(
                    formatter,
                    "truncated {codec:?} NAL header: {length} byte(s)"
                )
            }
            Self::ForbiddenBit { codec } => {
                write!(formatter, "{codec:?} NAL has forbidden_zero_bit set")
            }
        }
    }
}

impl std::error::Error for AccessUnitError {}

/// Splits one wire access unit into its NAL units, refusing anything that is
/// not well-formed Annex B: no leading start code, non-zero bytes before it,
/// a NAL header shorter than the codec's, or the forbidden bit set.
///
/// # Errors
///
/// Returns the first [`AccessUnitError`] found.
pub fn parse_access_unit(codec: NalCodec, data: &[u8]) -> Result<Vec<&[u8]>, AccessUnitError> {
    if data.is_empty() {
        return Err(AccessUnitError::Empty);
    }
    let Some((first_start_code, _)) = find_start_code(data, 0) else {
        return Err(AccessUnitError::MissingStartCode);
    };
    if data[..first_start_code].iter().any(|byte| *byte != 0) {
        return Err(AccessUnitError::DataBeforeStartCode);
    }
    let nals: Vec<&[u8]> = nal_units(data).collect();
    if nals.is_empty() {
        return Err(AccessUnitError::NoNalUnits);
    }
    let min_header_len = match codec {
        NalCodec::H264 => 1,
        NalCodec::H265 => 2,
    };
    for nal in &nals {
        if nal.len() < min_header_len {
            return Err(AccessUnitError::TruncatedHeader {
                codec,
                length: nal.len(),
            });
        }
        if nal[0] & 0x80 != 0 {
            return Err(AccessUnitError::ForbiddenBit { codec });
        }
    }
    Ok(nals)
}

/// Prefixes each NAL unit with its big-endian 32-bit length (the AVCC/HVCC
/// sample framing).
///
/// # Panics
///
/// Never in practice: a NAL unit longer than 4 GiB cannot arrive in a wire
/// packet. Such a unit is written with a saturated length instead.
#[must_use]
pub fn nals_to_length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
    let total = nals.iter().map(|nal| 4 + nal.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nal in nals {
        out.extend_from_slice(&u32::try_from(nal.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// What a NAL unit is, by its header's type field alone.
///
/// This is a role, not a validation: use [`nal_is_irap`] when a keyframe
/// claim must be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalKind {
    Slice,
    Keyframe,
    Vps,
    Sps,
    Pps,
    Other,
}

/// Classifies a NAL unit (starting at its header) by type.
#[must_use]
pub fn nal_kind(codec: NalCodec, nal: &[u8]) -> NalKind {
    match codec {
        NalCodec::H264 => match nal.first().map(|byte| byte & 0x1f) {
            Some(1) => NalKind::Slice,
            Some(5) => NalKind::Keyframe,
            Some(7) => NalKind::Sps,
            Some(8) => NalKind::Pps,
            _ => NalKind::Other,
        },
        // H.265 NAL type lives in bits 6..1 of the first header byte.
        NalCodec::H265 => match nal.first().map(|byte| (byte >> 1) & 0x3f) {
            Some(32) => NalKind::Vps,
            Some(33) => NalKind::Sps,
            Some(34) => NalKind::Pps,
            // IRAP pictures (BLA 16-18, IDR 19-20, CRA 21) are decoder
            // entry points — the H.265 analogue of an H.264 IDR.
            Some(nal_type) if (16..=21).contains(&nal_type) => NalKind::Keyframe,
            Some(nal_type) if nal_type <= 31 => NalKind::Slice,
            _ => NalKind::Other,
        },
    }
}

/// What [`ParameterSetCache::observe`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParameterSetUpdate {
    /// The access unit carried a parameter set that changed the cached
    /// configuration (or the codec switched); a decoder session built from
    /// the old one must be rebuilt.
    pub changed: bool,
    /// The cached SPS was replaced by a different one.
    pub new_sps: bool,
}

/// The newest VPS/SPS/PPS of a stream, for decoders that are configured from
/// a complete parameter set and want each keyframe to carry one.
#[derive(Debug, Default, Clone)]
pub struct ParameterSetCache {
    codec: Option<NalCodec>,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParameterSetCache {
    /// Forgets every cached set and binds the cache to `codec`.
    pub fn reset(&mut self, codec: NalCodec) {
        self.codec = Some(codec);
        self.vps = None;
        self.sps = None;
        self.pps = None;
    }

    /// The cached SPS, if any.
    #[must_use]
    pub fn sps(&self) -> Option<&[u8]> {
        self.sps.as_deref()
    }

    /// Records the parameter sets carried by one access unit's NAL units.
    pub fn observe(&mut self, codec: NalCodec, nals: &[&[u8]]) -> ParameterSetUpdate {
        let mut changed = false;
        let mut new_sps = false;
        if self.codec != Some(codec) {
            self.reset(codec);
            changed = true;
        }
        let newest = |kind: NalKind| {
            nals.iter()
                .rev()
                .find(|nal| nal_kind(codec, nal) == kind)
                .copied()
        };
        let (vps_in, sps_in, pps_in) = (
            newest(NalKind::Vps),
            newest(NalKind::Sps),
            newest(NalKind::Pps),
        );
        let has_incoming = vps_in.is_some() || sps_in.is_some() || pps_in.is_some();
        let differs = |incoming: Option<&[u8]>, cached: &Option<Vec<u8>>| {
            incoming.is_some_and(|nal| cached.as_deref().is_some_and(|old| old != nal))
        };
        let config_changed =
            differs(vps_in, &self.vps) || differs(sps_in, &self.sps) || differs(pps_in, &self.pps);

        if config_changed && self.is_complete(codec) {
            self.reset(codec);
            changed = true;
        }
        if let Some(vps) = vps_in {
            if self.vps.as_deref() != Some(vps) {
                self.vps = Some(vps.to_vec());
                changed = true;
            }
        }
        if let Some(sps) = sps_in {
            if self.sps.as_deref() != Some(sps) {
                self.sps = Some(sps.to_vec());
                changed = true;
                new_sps = true;
            }
        }
        if let Some(pps) = pps_in {
            if self.pps.as_deref() != Some(pps) {
                self.pps = Some(pps.to_vec());
                changed = true;
            }
        }
        ParameterSetUpdate {
            changed: changed && has_incoming,
            new_sps,
        }
    }

    /// Whether every set `codec` needs is cached.
    #[must_use]
    pub const fn is_complete(&self, codec: NalCodec) -> bool {
        match codec {
            NalCodec::H264 => self.sps.is_some() && self.pps.is_some(),
            NalCodec::H265 => self.vps.is_some() && self.sps.is_some() && self.pps.is_some(),
        }
    }

    /// The complete set in decoder order, when cached for `codec`.
    #[must_use]
    pub fn parameter_sets(&self, codec: NalCodec) -> Option<Vec<&[u8]>> {
        if self.codec != Some(codec) || !self.is_complete(codec) {
            return None;
        }
        Some(match codec {
            NalCodec::H264 => vec![self.sps.as_deref()?, self.pps.as_deref()?],
            NalCodec::H265 => vec![
                self.vps.as_deref()?,
                self.sps.as_deref()?,
                self.pps.as_deref()?,
            ],
        })
    }
}

/// Why [`prepare_sample_nals`] could not build a decodable sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplePreparationError {
    /// A keyframe arrived before a complete parameter set.
    MissingParameterSets,
}

/// The NAL units to hand a decoder for one access unit: its slices, with the
/// cached complete parameter set in front of a keyframe (inline parameter
/// sets are replaced by the canonical cached copy, so each keyframe carries
/// exactly one). `Ok(None)` when the unit has no slice.
///
/// # Errors
///
/// [`SamplePreparationError::MissingParameterSets`] for a keyframe without a
/// complete cached set.
pub fn prepare_sample_nals<'a>(
    codec: NalCodec,
    cache: &'a ParameterSetCache,
    nals: &'a [&'a [u8]],
) -> Result<Option<Vec<&'a [u8]>>, SamplePreparationError> {
    let has_keyframe = nals
        .iter()
        .any(|nal| nal_kind(codec, nal) == NalKind::Keyframe);
    let slices: Vec<&[u8]> = nals
        .iter()
        .copied()
        .filter(|nal| matches!(nal_kind(codec, nal), NalKind::Slice | NalKind::Keyframe))
        .collect();
    if slices.is_empty() {
        return Ok(None);
    }
    if !has_keyframe {
        return Ok(Some(slices));
    }
    let Some(parameter_sets) = cache.parameter_sets(codec) else {
        return Err(SamplePreparationError::MissingParameterSets);
    };
    let mut prepared = Vec::with_capacity(parameter_sets.len() + slices.len());
    prepared.extend(parameter_sets);
    prepared.extend(slices);
    Ok(Some(prepared))
}

/// What one Annex B access unit carries, for keyframe and recovery decisions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct AccessUnitSummary {
    /// An IRAP (H.265) or IDR (H.264) picture, by [`nal_is_irap`].
    pub irap: bool,
    /// An H.265 video parameter set.
    pub vps: bool,
    /// A sequence parameter set.
    pub sps: bool,
    /// A picture parameter set.
    pub pps: bool,
}

impl AccessUnitSummary {
    /// Whether a decoder with no state can start here: a keyframe together
    /// with every parameter set the codec needs to decode it.
    #[must_use]
    pub const fn is_recovery_point(self, codec: NalCodec) -> bool {
        self.irap
            && self.sps
            && self.pps
            && match codec {
                NalCodec::H264 => true,
                NalCodec::H265 => self.vps,
            }
    }
}

/// Reads one Annex B access unit, or refuses it.
///
/// The unit must be well-formed Annex B ([`parse_access_unit`]: a leading
/// start code with nothing but zero bytes before it) and every NAL unit's
/// header must be valid for `codec` ([`nal_header_is_valid`]). One bad header
/// makes the whole unit `None`: a unit carrying a corrupt or foreign NAL is
/// not one to tell a client it can start decoding at. This is also what
/// catches a unit of the other codec, whose headers do not parse as this one.
///
/// A parameter set counts only with a payload and, for H.264, a non-zero
/// `nal_ref_idc` (both are required of a real SPS or PPS).
#[must_use]
pub fn summarize_access_unit(access_unit: &[u8], codec: NalCodec) -> Option<AccessUnitSummary> {
    let nals = parse_access_unit(codec, access_unit).ok()?;
    let header_len = match codec {
        NalCodec::H264 => 1,
        NalCodec::H265 => 2,
    };
    let mut summary = AccessUnitSummary::default();
    for nal in nals {
        if !nal_header_is_valid(nal, codec) {
            return None;
        }
        summary.irap |= nal_is_irap(nal, codec);
        if nal.len() > header_len && nal_is_parameter_set(nal, codec) {
            match nal_kind(codec, nal) {
                NalKind::Vps => summary.vps = true,
                NalKind::Sps => summary.sps = true,
                NalKind::Pps => summary.pps = true,
                NalKind::Slice | NalKind::Keyframe | NalKind::Other => {}
            }
        }
    }
    Some(summary)
}

/// Returns whether an Annex B access unit is a keyframe: well-formed, and
/// carrying an IRAP (H.265) or IDR (H.264) picture. Every NAL unit is
/// scanned, because the picture follows the parameter sets and any SEI.
#[must_use]
pub fn access_unit_is_keyframe(access_unit: &[u8], codec: NalCodec) -> bool {
    summarize_access_unit(access_unit, codec).is_some_and(|summary| summary.irap)
}

/// Returns whether an Annex B access unit is a self-contained recovery point:
/// a keyframe that also carries every parameter set needed to decode it.
#[must_use]
pub fn access_unit_is_recovery_point(access_unit: &[u8], codec: NalCodec) -> bool {
    summarize_access_unit(access_unit, codec)
        .is_some_and(|summary| summary.is_recovery_point(codec))
}

impl NalCodec {
    /// The NAL layout of a negotiated codec, or `None` for a codec that has
    /// none (AV1 is a sequence of OBUs; JPEG and VP9 are not Annex B).
    #[must_use]
    pub const fn from_video_codec(codec: crate::VideoCodec) -> Option<Self> {
        match codec {
            crate::VideoCodec::H264 => Some(Self::H264),
            crate::VideoCodec::H265 => Some(Self::H265),
            crate::VideoCodec::Jpeg | crate::VideoCodec::Vp9 | crate::VideoCodec::Av1 => None,
        }
    }
}

/// Returns whether one encoded unit of `codec`, as a Pier's encoder writes it
/// (an Annex B access unit for H.264 and H.265, a low-overhead temporal unit
/// for AV1), is a keyframe.
///
/// AV1 counts as a keyframe when the temporal unit carries a sequence header:
/// every Arcen AV1 encoder repeats it on each key frame, and without it a
/// joining client cannot decode anyway. JPEG and VP9 are never classified
/// here and are `false`.
#[must_use]
pub fn video_unit_is_keyframe(unit: &[u8], codec: crate::VideoCodec) -> bool {
    match codec {
        crate::VideoCodec::Av1 => crate::video::av1_low_overhead_has_sequence_header(unit),
        crate::VideoCodec::H264 => access_unit_is_keyframe(unit, NalCodec::H264),
        crate::VideoCodec::H265 => access_unit_is_keyframe(unit, NalCodec::H265),
        crate::VideoCodec::Jpeg | crate::VideoCodec::Vp9 => false,
    }
}

/// Returns whether one encoded unit of `codec` is a self-contained recovery
/// point ([`access_unit_is_recovery_point`]; for AV1, the same sequence-header
/// rule as [`video_unit_is_keyframe`]).
#[must_use]
pub fn video_unit_is_recovery_point(unit: &[u8], codec: crate::VideoCodec) -> bool {
    match codec {
        crate::VideoCodec::Av1 => crate::video::av1_low_overhead_has_sequence_header(unit),
        crate::VideoCodec::H264 => access_unit_is_recovery_point(unit, NalCodec::H264),
        crate::VideoCodec::H265 => access_unit_is_recovery_point(unit, NalCodec::H265),
        crate::VideoCodec::Jpeg | crate::VideoCodec::Vp9 => false,
    }
}

/// The parameter sets (H.264 SPS/PPS, H.265 VPS/SPS/PPS) in an Annex B
/// buffer, in stream order and without start codes. A buffer with no start
/// code is read as one bare NAL unit ([`split_annex_b`]).
#[must_use]
pub fn annex_b_parameter_sets(data: &[u8], codec: NalCodec) -> Vec<&[u8]> {
    split_annex_b(data)
        .into_iter()
        .filter(|nal| nal_is_parameter_set(nal, codec))
        .collect()
}

/// Puts `units`, each behind a four-byte start code, in front of an Annex B
/// access unit: how an encoder that reports its parameter sets out of band
/// makes each keyframe self-contained.
pub fn prepend_annex_b_units<U: AsRef<[u8]>>(units: &[U], access_unit: &mut Vec<u8>) {
    if units.is_empty() {
        return;
    }
    let mut prefix = Vec::with_capacity(
        units
            .iter()
            .map(|unit| START_CODE.len() + unit.as_ref().len())
            .sum(),
    );
    for unit in units {
        prefix.extend_from_slice(&START_CODE);
        prefix.extend_from_slice(unit.as_ref());
    }
    access_unit.splice(0..0, prefix);
}

/// Reads the SPS and PPS NAL units out of an `AVCDecoderConfigurationRecord`
/// (ISO/IEC 14496-15 §5.3.3.1), in record order, SPS first.
///
/// ```text
/// [1] configurationVersion (= 1)
/// [3] AVCProfileIndication, profile_compatibility, AVCLevelIndication
/// [1] 0b111111 | lengthSizeMinusOne
/// [1] 0b111 | numOfSequenceParameterSets
///     per SPS: [2] u16 length, [length] NAL unit
/// [1] numOfPictureParameterSets
///     per PPS: [2] u16 length, [length] NAL unit
/// [..] profile-dependent extension, ignored
/// ```
///
/// Returns `None` for a record that is not version 1, is truncated, or does
/// not hold at least one SPS (type 7) and one PPS (type 8) with nothing else
/// in their places: half a configuration is not one a decoder can use, and a
/// caller that gets `None` can still look for the sets in the stream.
#[must_use]
pub fn parse_avc_decoder_config(record: &[u8]) -> Option<Vec<&[u8]>> {
    fn take_units<'a>(
        record: &'a [u8],
        offset: &mut usize,
        count: usize,
        kind: NalKind,
        out: &mut Vec<&'a [u8]>,
    ) -> Option<()> {
        for _ in 0..count {
            let length_end = offset.checked_add(2)?;
            let length = record.get(*offset..length_end)?;
            let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
            let end = length_end.checked_add(length)?;
            let unit = record.get(length_end..end)?;
            if unit.len() < 2
                || !nal_is_parameter_set(unit, NalCodec::H264)
                || nal_kind(NalCodec::H264, unit) != kind
            {
                return None;
            }
            out.push(unit);
            *offset = end;
        }
        Some(())
    }
    if *record.first()? != 1 {
        return None;
    }
    let sps_count = usize::from(record.get(5)? & 0x1F);
    let mut offset = 6;
    let mut units = Vec::new();
    take_units(record, &mut offset, sps_count, NalKind::Sps, &mut units)?;
    let pps_count = usize::from(*record.get(offset)?);
    offset += 1;
    take_units(record, &mut offset, pps_count, NalKind::Pps, &mut units)?;
    (sps_count > 0 && pps_count > 0).then_some(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn length_prefixed(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&u32::try_from(unit.len()).expect("fits").to_be_bytes());
            out.extend_from_slice(unit);
        }
        out
    }

    #[test]
    fn an_h264_p_slice_is_not_a_keyframe_despite_aliasing_an_hevc_irap() {
        // This is the bug this module exists to prevent. 0x21 is an ordinary
        // H.264 P-slice header, and it reads as HEVC nal_unit_type 16, inside
        // the IRAP range. Code that tries both layouts reports every H.264
        // frame as a keyframe, which looks fine until a client reconnects.
        // As H.264: forbidden bit clear, nal_ref_idc 1, type 1 - a P-slice.
        // As H.265: type (0x21 >> 1) & 0x3F = 16, but its low bit makes
        // nuh_layer_id at least 32, so the base-layer rule refuses it too.
        let sample = length_prefixed(&[&[0x21, 0xA9, 0xBB]]);
        assert!(!length_prefixed_is_keyframe(&sample, NalCodec::H264));
        assert!(!length_prefixed_is_keyframe(&sample, NalCodec::H265));

        // An even header byte does alias a base-layer IRAP: 0x28 is an H.264
        // PPS and HEVC type 20 (IDR_N_LP). The same three bytes, two valid
        // readings, so only the negotiated codec can decide.
        let sample = length_prefixed(&[&[0x28, 0x01, 0xBB]]);
        assert!(!length_prefixed_is_keyframe(&sample, NalCodec::H264));
        assert!(length_prefixed_is_keyframe(&sample, NalCodec::H265));
    }

    #[test]
    fn h264_idr_and_hevc_irap_are_keyframes() {
        let idr = length_prefixed(&[&[0x65, 0x01]]);
        assert!(length_prefixed_is_keyframe(&idr, NalCodec::H264));

        // nal_unit_type 19 (IDR_W_RADL) is 19 << 1 = 0x26, with
        // temporal_id_plus_one 1 in the second header byte.
        let irap = length_prefixed(&[&[0x26, 0x01, 0x00]]);
        assert!(length_prefixed_is_keyframe(&irap, NalCodec::H265));
    }

    #[test]
    fn conversion_preserves_every_unit_and_its_bytes() {
        let sample = length_prefixed(&[&[0x67, 0x01, 0x02], &[0x68, 0x03], &[0x65, 0x04]]);
        let mut out = Vec::new();
        append_length_prefixed_as_annex_b(&mut out, &sample);

        let mut units: Vec<Vec<u8>> = Vec::new();
        for_each_annex_b(&out, |nal| units.push(nal.to_vec()));
        assert_eq!(
            units,
            vec![vec![0x67, 0x01, 0x02], vec![0x68, 0x03], vec![0x65, 0x04],]
        );
        assert!(access_unit_is_keyframe(&out, NalCodec::H264));
    }

    #[test]
    fn a_truncated_length_stops_rather_than_reading_past_the_sample() {
        // A length claiming more bytes than exist must not panic or read
        // beyond the buffer; the valid prefix is kept.
        let mut sample = length_prefixed(&[&[0x65, 0x01]]);
        sample.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF, 0x41]);
        let mut out = Vec::new();
        append_length_prefixed_as_annex_b(&mut out, &sample);
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0x01]);
        assert!(length_prefixed_is_keyframe(&sample, NalCodec::H264));
    }

    #[test]
    fn a_zero_length_unit_is_skipped_and_the_walk_goes_on() {
        // Some Media Foundation transforms emit an empty tail. Four bytes of
        // zero length describe themselves exactly, so the units after them
        // are still read; nothing is emitted for the empty one.
        let mut sample = vec![0, 0, 0, 0];
        sample.extend_from_slice(&length_prefixed(&[&[0x65, 0x01]]));
        sample.extend_from_slice(&[0, 0, 0, 0]);
        let mut out = Vec::new();
        append_length_prefixed_as_annex_b(&mut out, &sample);
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0x01]);
        let mut checked = Vec::new();
        assert_eq!(length_prefixed_to_annex_b(&sample, &mut checked), Ok(()));
        assert_eq!(checked, out);
        assert!(length_prefixed_is_keyframe(&sample, NalCodec::H264));
    }

    #[test]
    fn the_checked_conversion_refuses_a_malformed_sample_whole() {
        let mut truncated = length_prefixed(&[&[0x67, 0x42]]);
        truncated.extend_from_slice(&[0, 0, 0]);
        let mut out = vec![0xAA];
        assert_eq!(
            length_prefixed_to_annex_b(&truncated, &mut out),
            Err(LengthPrefixedError::TruncatedLength)
        );
        assert_eq!(out, [0xAA], "nothing of a refused sample is kept");

        let mut past_end = length_prefixed(&[&[0x67, 0x42]]);
        past_end.extend_from_slice(&[0, 0, 0, 10, 0x65, 0xAA]);
        assert_eq!(
            length_prefixed_to_annex_b(&past_end, &mut out),
            Err(LengthPrefixedError::LengthPastEnd)
        );
        assert_eq!(out, [0xAA]);

        let mut multiple = Vec::new();
        length_prefixed_to_annex_b(
            &length_prefixed(&[&[0x67, 0x42], &[0x68, 0xCE, 0x38]]),
            &mut multiple,
        )
        .expect("well-formed");
        assert_eq!(
            multiple,
            [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0x38]
        );
    }

    #[test]
    fn three_and_four_byte_start_codes_are_both_read() {
        // Real bitstreams mix them, and a scanner that only knows one silently
        // merges two units into one.
        let mut stream = vec![0, 0, 0, 1, 0x67, 0x11];
        stream.extend_from_slice(&[0, 0, 1, 0x65, 0x22]);
        let mut units: Vec<Vec<u8>> = Vec::new();
        for_each_annex_b(&stream, |nal| units.push(nal.to_vec()));
        assert_eq!(units, vec![vec![0x67, 0x11], vec![0x65, 0x22]]);
        assert!(access_unit_is_keyframe(&stream, NalCodec::H264));
        assert!(starts_with_start_code(&stream));
        assert!(starts_with_start_code(&stream[1..]));
        assert!(!starts_with_start_code(&stream[2..]));
        assert!(!starts_with_start_code(&[0, 0]));
    }

    #[test]
    fn malformed_headers_are_not_read_as_keyframes() {
        // The forbidden zero bit set means this is not a NAL unit. Masking it
        // away and trusting the result turns a corrupt byte into a confidently
        // reported keyframe.
        let forbidden = length_prefixed(&[&[0xE5, 0x01]]);
        assert!(!length_prefixed_is_keyframe(&forbidden, NalCodec::H264));

        // An IDR marked disposable is a contradiction.
        let disposable = length_prefixed(&[&[0x05, 0x01]]);
        assert!(!length_prefixed_is_keyframe(&disposable, NalCodec::H264));

        // A slice header with no payload.
        let empty_body = length_prefixed(&[&[0x65]]);
        assert!(!length_prefixed_is_keyframe(&empty_body, NalCodec::H264));

        // HEVC reserved IRAP types 22 and 23 are not keyframes to start at:
        // their meaning is not defined, so telling a client to begin decoding
        // there is worse than waiting for a real one. 22 << 1 = 0x2C.
        let reserved = length_prefixed(&[&[0x2C, 0x01, 0x00]]);
        assert!(!length_prefixed_is_keyframe(&reserved, NalCodec::H265));

        // An IRAP is always at the base temporal layer.
        let higher_layer = length_prefixed(&[&[0x26, 0x02, 0x00]]);
        assert!(!length_prefixed_is_keyframe(&higher_layer, NalCodec::H265));
    }

    #[test]
    fn parameter_sets_are_recognised_for_both_codecs() {
        assert!(nal_is_parameter_set(&[0x67], NalCodec::H264));
        assert!(nal_is_parameter_set(&[0x68], NalCodec::H264));
        assert!(!nal_is_parameter_set(&[0x65], NalCodec::H264));

        // 32 << 1 = 0x40 (VPS), 33 << 1 = 0x42 (SPS), 34 << 1 = 0x44 (PPS).
        assert!(nal_is_parameter_set(&[0x40], NalCodec::H265));
        assert!(nal_is_parameter_set(&[0x42], NalCodec::H265));
        assert!(nal_is_parameter_set(&[0x44], NalCodec::H265));
        assert!(!nal_is_parameter_set(&[0x26], NalCodec::H265));
    }

    #[test]
    fn an_empty_unit_is_not_classified_as_anything() {
        assert!(!nal_is_irap(&[], NalCodec::H264));
        assert!(!nal_is_irap(&[], NalCodec::H265));
        assert!(!nal_is_parameter_set(&[], NalCodec::H264));
    }

    #[test]
    fn splits_annex_b_nals_with_three_and_four_byte_start_codes() {
        let bytes = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
        ];
        let nals = split_annex_b(&bytes);
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]
        );
        assert_eq!(nal_kind(NalCodec::H264, nals[0]), NalKind::Sps);
        assert_eq!(nal_kind(NalCodec::H264, nals[1]), NalKind::Pps);
        assert_eq!(nal_kind(NalCodec::H264, nals[2]), NalKind::Keyframe);
    }

    #[test]
    fn converts_annex_b_nals_to_avcc_length_prefixed_payload() {
        let nals = vec![&[0x65, 1, 2][..], &[0x41, 3][..]];
        let avcc = nals_to_length_prefixed(&nals);
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x65, 1, 2, 0, 0, 0, 2, 0x41, 3]);
    }

    #[test]
    fn classifies_hevc_nal_types() {
        // First header byte = nal_type << 1 (layer/temporal bits zero).
        assert_eq!(nal_kind(NalCodec::H265, &[32 << 1, 1]), NalKind::Vps);
        assert_eq!(nal_kind(NalCodec::H265, &[33 << 1, 1]), NalKind::Sps);
        assert_eq!(nal_kind(NalCodec::H265, &[34 << 1, 1]), NalKind::Pps);
        // IDR_W_RADL (19) and CRA (21) are keyframes.
        assert_eq!(nal_kind(NalCodec::H265, &[19 << 1, 1]), NalKind::Keyframe);
        assert_eq!(nal_kind(NalCodec::H265, &[21 << 1, 1]), NalKind::Keyframe);
        // TRAIL_R (1) is a plain slice.
        assert_eq!(nal_kind(NalCodec::H265, &[1 << 1, 1]), NalKind::Slice);
        // SEI (39) is not a slice.
        assert_eq!(nal_kind(NalCodec::H265, &[39 << 1, 1]), NalKind::Other);
    }

    #[test]
    fn h264_keyframes_receive_one_complete_cached_parameter_set() {
        let sps = &[0x67, 0x64, 0x00][..];
        let pps = &[0x68, 0xee][..];
        let idr = &[0x65, 0x88][..];
        let mut cache = ParameterSetCache::default();
        assert!(cache.observe(NalCodec::H264, &[sps, pps]).changed);

        let bare_idr = [idr];
        let inline_idr = [sps, pps, idr];
        let injected = prepare_sample_nals(NalCodec::H264, &cache, &bare_idr)
            .unwrap()
            .unwrap();
        assert_eq!(injected, vec![sps, pps, idr]);

        let canonicalized = prepare_sample_nals(NalCodec::H264, &cache, &inline_idr)
            .unwrap()
            .unwrap();
        assert_eq!(canonicalized, vec![sps, pps, idr]);

        // A rebuilt decoder session can reuse the active stream cache.
        let recovered = prepare_sample_nals(NalCodec::H264, &cache, &bare_idr)
            .unwrap()
            .unwrap();
        assert_eq!(recovered, vec![sps, pps, idr]);
    }

    #[test]
    fn h264_config_change_and_reset_require_a_complete_new_parameter_set() {
        let old_sps = &[0x67, 1][..];
        let old_pps = &[0x68, 1][..];
        let new_sps = &[0x67, 2][..];
        let new_pps = &[0x68, 2][..];
        let idr = &[0x65, 9][..];
        let mut cache = ParameterSetCache::default();
        cache.observe(NalCodec::H264, &[old_sps, old_pps]);

        assert!(cache.observe(NalCodec::H264, &[new_sps]).changed);
        assert!(!cache.is_complete(NalCodec::H264));
        assert_eq!(
            prepare_sample_nals(NalCodec::H264, &cache, &[idr]),
            Err(SamplePreparationError::MissingParameterSets)
        );
        cache.observe(NalCodec::H264, &[new_pps]);
        assert_eq!(
            prepare_sample_nals(NalCodec::H264, &cache, &[idr])
                .unwrap()
                .unwrap(),
            vec![new_sps, new_pps, idr]
        );

        cache.reset(NalCodec::H264);
        assert_eq!(
            prepare_sample_nals(NalCodec::H264, &cache, &[idr]),
            Err(SamplePreparationError::MissingParameterSets)
        );
    }

    #[test]
    fn hevc_keyframes_receive_vps_sps_pps_once_and_codec_switch_clears_cache() {
        let vps = &[32 << 1, 1][..];
        let sps = &[33 << 1, 1][..];
        let pps = &[34 << 1, 1][..];
        let idr = &[19 << 1, 1][..];
        let mut cache = ParameterSetCache::default();
        cache.observe(NalCodec::H265, &[vps, sps, pps]);

        assert_eq!(
            prepare_sample_nals(NalCodec::H265, &cache, &[idr])
                .unwrap()
                .unwrap(),
            vec![vps, sps, pps, idr]
        );
        assert_eq!(
            prepare_sample_nals(NalCodec::H265, &cache, &[vps, sps, pps, idr])
                .unwrap()
                .unwrap(),
            vec![vps, sps, pps, idr]
        );

        cache.observe(NalCodec::H264, &[]);
        assert!(!cache.is_complete(NalCodec::H264));
        assert!(cache.parameter_sets(NalCodec::H265).is_none());
    }

    #[test]
    fn non_keyframes_are_not_injected_and_malformed_access_units_fail() {
        let sps = &[0x67, 1][..];
        let pps = &[0x68, 1][..];
        let slice = &[0x41, 7][..];
        let mut cache = ParameterSetCache::default();
        cache.observe(NalCodec::H264, &[sps, pps]);
        assert_eq!(
            prepare_sample_nals(NalCodec::H264, &cache, &[slice])
                .unwrap()
                .unwrap(),
            vec![slice]
        );

        assert!(parse_access_unit(NalCodec::H264, &[0x65, 1]).is_err());
        assert!(parse_access_unit(NalCodec::H265, &[0, 0, 0, 1, 32 << 1]).is_err());
        assert!(parse_access_unit(NalCodec::H264, &[]).is_err());

        let padded = [0, 0, 0, 0, 1, 0x65, 1, 0, 0];
        assert_eq!(
            parse_access_unit(NalCodec::H264, &padded).unwrap(),
            vec![&[0x65, 1][..]]
        );
    }

    #[test]
    fn nal_units_trims_trailing_zeros_and_skips_zero_only_units() {
        let stream = [
            0, 0, 0, 1, 0x67, 0x11, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0x65, 0x22, 0,
        ];
        let units: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(units, vec![&[0x67, 0x11][..], &[0x65, 0x22][..]]);
        assert!(nal_units(&[1, 2, 3]).next().is_none());
        assert_eq!(split_annex_b(&[0x67, 1]), vec![&[0x67, 1][..]]);
    }

    #[test]
    fn access_unit_errors_keep_their_wording() {
        assert_eq!(
            parse_access_unit(NalCodec::H264, &[])
                .unwrap_err()
                .to_string(),
            "empty Annex-B access unit"
        );
        assert_eq!(
            parse_access_unit(NalCodec::H264, &[1, 0, 0, 1, 0x65])
                .unwrap_err()
                .to_string(),
            "Annex-B access unit has non-zero data before its first start code"
        );
        assert_eq!(
            parse_access_unit(NalCodec::H264, &[0, 0, 1, 0xe5])
                .unwrap_err()
                .to_string(),
            "H264 NAL has forbidden_zero_bit set"
        );
    }

    // ---- Access-unit classification: the one rule every Pier uses ----

    fn annex_b(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(unit);
        }
        out
    }

    /// An H.264 NAL unit: header from `nal_ref_idc` and type, one payload byte.
    fn h264(nal_ref_idc: u8, nal_type: u8) -> Vec<u8> {
        vec![(nal_ref_idc << 5) | nal_type, 0x88]
    }

    /// An H.265 NAL unit: two-byte header, one payload byte.
    fn h265(nal_type: u8, layer: u8, temporal_id_plus_one: u8) -> Vec<u8> {
        vec![
            (nal_type << 1) | (layer >> 5),
            ((layer & 0x1F) << 3) | temporal_id_plus_one,
            0x80,
        ]
    }

    #[test]
    fn every_h264_nal_type_is_pinned() {
        // ITU-T H.264 Table 7-1. Only an IDR slice (5) starts decoding; 0 and
        // 24..=31 are unspecified and make the whole access unit unreadable.
        for nal_type in 0..=31_u8 {
            let unit = annex_b(&[&h264(3, nal_type)]);
            let summary = summarize_access_unit(&unit, NalCodec::H264);
            match nal_type {
                0 | 24..=31 => assert_eq!(summary, None, "type {nal_type}"),
                5 => assert!(summary.expect("valid").irap, "type {nal_type}"),
                _ => assert!(!summary.expect("valid").irap, "type {nal_type}"),
            }
            assert_eq!(
                access_unit_is_keyframe(&unit, NalCodec::H264),
                nal_type == 5,
                "type {nal_type}"
            );
        }
        // A disposable IDR (nal_ref_idc 0) is a contradiction, not a keyframe.
        assert!(!access_unit_is_keyframe(
            &annex_b(&[&h264(0, 5)]),
            NalCodec::H264
        ));
    }

    #[test]
    fn every_h265_nal_type_is_pinned() {
        // ITU-T H.265 Table 7-1. BLA (16..=18), IDR (19, 20) and CRA (21) are
        // the IRAP pictures decoding may start at; 22 and 23 are reserved
        // IRAP types; 48..=63 are unspecified and make the unit unreadable.
        for nal_type in 0..=63_u8 {
            let unit = annex_b(&[&h265(nal_type, 0, 1)]);
            let summary = summarize_access_unit(&unit, NalCodec::H265);
            match nal_type {
                48..=63 => assert_eq!(summary, None, "type {nal_type}"),
                16..=21 => assert!(summary.expect("valid").irap, "type {nal_type}"),
                _ => assert!(!summary.expect("valid").irap, "type {nal_type}"),
            }
            assert_eq!(
                access_unit_is_keyframe(&unit, NalCodec::H265),
                (16..=21).contains(&nal_type),
                "type {nal_type}"
            );
        }
    }

    #[test]
    fn an_h265_irap_must_be_base_layer_and_base_temporal_layer() {
        for nal_type in 16..=21 {
            assert!(!access_unit_is_keyframe(
                &annex_b(&[&h265(nal_type, 0, 2)]),
                NalCodec::H265
            ));
            // A single-layer decoder discards every nuh_layer_id > 0 unit, so
            // an enhancement-layer IRAP is nothing it can start from. Layer 1
            // sets the low bit of byte 1's top five bits; layer 32 sets byte
            // 0's low bit.
            for layer in [1, 32, 63] {
                assert!(
                    !access_unit_is_keyframe(
                        &annex_b(&[&h265(nal_type, layer, 1)]),
                        NalCodec::H265
                    ),
                    "type {nal_type} layer {layer}"
                );
            }
        }
        // A zero temporal_id_plus_one is forbidden outright.
        assert_eq!(
            summarize_access_unit(&annex_b(&[&h265(19, 0, 0)]), NalCodec::H265),
            None
        );
    }

    #[test]
    fn recovery_points_need_every_parameter_set() {
        let (sps, pps, idr) = (h264(3, 7), h264(3, 8), h264(3, 5));
        assert!(access_unit_is_recovery_point(
            &annex_b(&[&sps, &pps, &h264(0, 6), &idr]),
            NalCodec::H264
        ));
        assert!(!access_unit_is_recovery_point(
            &annex_b(&[&idr]),
            NalCodec::H264
        ));
        assert!(!access_unit_is_recovery_point(
            &annex_b(&[&sps, &idr]),
            NalCodec::H264
        ));
        // Parameter sets marked disposable are not parameter sets.
        assert!(!access_unit_is_recovery_point(
            &annex_b(&[&h264(0, 7), &h264(0, 8), &idr]),
            NalCodec::H264
        ));
        // A parameter-set header with no payload is not a parameter set.
        assert!(!access_unit_is_recovery_point(
            &annex_b(&[&[0x67], &pps, &idr]),
            NalCodec::H264
        ));

        let (vps, sps, pps) = (h265(32, 0, 1), h265(33, 0, 1), h265(34, 0, 1));
        for irap in 16..=21 {
            let picture = h265(irap, 0, 1);
            assert!(access_unit_is_recovery_point(
                &annex_b(&[&vps, &sps, &pps, &h265(39, 0, 1), &picture]),
                NalCodec::H265
            ));
            assert!(!access_unit_is_recovery_point(
                &annex_b(&[&sps, &pps, &picture]),
                NalCodec::H265
            ));
        }
        assert!(!access_unit_is_recovery_point(
            &annex_b(&[&vps, &sps, &pps, &h265(1, 0, 1)]),
            NalCodec::H265
        ));
    }

    #[test]
    fn access_unit_framing_follows_annex_b() {
        let idr = h264(3, 5);
        // Leading zero bytes (leading_zero_8bits) and trailing zeros
        // (trailing_zero_8bits) are part of the byte stream, not the unit.
        let mut padded = vec![0, 0];
        padded.extend(annex_b(&[&idr]));
        padded.extend([0, 0]);
        assert!(access_unit_is_keyframe(&padded, NalCodec::H264));
        // Mixed three- and four-byte start codes.
        let mut mixed = annex_b(&[&h264(3, 7)]);
        mixed.extend([0, 0, 1]);
        mixed.extend(&idr);
        assert!(access_unit_is_keyframe(&mixed, NalCodec::H264));
        // Anything else before the first start code is not Annex B.
        let mut junk = vec![0x42];
        junk.extend(annex_b(&[&idr]));
        assert!(!access_unit_is_keyframe(&junk, NalCodec::H264));
        assert!(!access_unit_is_keyframe(&idr, NalCodec::H264));
        assert!(!access_unit_is_keyframe(&[], NalCodec::H264));
        assert!(!access_unit_is_keyframe(&START_CODE, NalCodec::H264));
        // One corrupt header spoils the unit, wherever it is.
        let corrupt = annex_b(&[&h264(3, 7), &[0xE5, 0x01], &idr]);
        assert!(!access_unit_is_keyframe(&corrupt, NalCodec::H264));
        // A header-only end of sequence or end of bitstream is legal and
        // does not spoil the unit (H.264 10/11, H.265 36/37).
        assert!(access_unit_is_keyframe(
            &annex_b(&[&idr, &[0x0A]]),
            NalCodec::H264
        ));
        assert!(access_unit_is_keyframe(
            &annex_b(&[&h265(19, 0, 1), &[36 << 1, 1]]),
            NalCodec::H265
        ));
    }

    #[test]
    fn a_unit_of_the_other_codec_is_never_a_keyframe() {
        // An H.265 unit read as H.264: VPS (0x40) is H.264 type 0.
        let hevc = annex_b(&[
            &h265(32, 0, 1),
            &h265(33, 0, 1),
            &h265(34, 0, 1),
            &h265(19, 0, 1),
        ]);
        assert!(access_unit_is_recovery_point(&hevc, NalCodec::H265));
        assert!(!access_unit_is_keyframe(&hevc, NalCodec::H264));
        // An H.264 unit read as H.265: SPS 0x67 is H.265 type 51.
        let avc = annex_b(&[&h264(3, 7), &h264(3, 8), &h264(3, 5)]);
        assert!(access_unit_is_recovery_point(&avc, NalCodec::H264));
        assert!(!access_unit_is_keyframe(&avc, NalCodec::H265));
    }

    #[test]
    fn video_units_follow_their_codec() {
        use crate::VideoCodec;

        let avc = annex_b(&[&h264(3, 7), &h264(3, 8), &h264(3, 5)]);
        assert!(video_unit_is_keyframe(&avc, VideoCodec::H264));
        assert!(video_unit_is_recovery_point(&avc, VideoCodec::H264));
        assert!(!video_unit_is_keyframe(&avc, VideoCodec::H265));
        let bla = annex_b(&[&h265(16, 0, 1)]);
        assert!(video_unit_is_keyframe(&bla, VideoCodec::H265));
        assert!(!video_unit_is_recovery_point(&bla, VideoCodec::H265));

        // AV1: a temporal unit with a sequence header OBU (type 1).
        let obu = |obu_type: u8, payload: &[u8]| {
            let mut out = vec![
                (obu_type << 3) | 0x02,
                u8::try_from(payload.len()).expect("short"),
            ];
            out.extend_from_slice(payload);
            out
        };
        let mut temporal_unit = obu(2, &[]);
        temporal_unit.extend(obu(1, &[0x10, 0x20]));
        temporal_unit.extend(obu(6, &[0x30]));
        assert!(video_unit_is_keyframe(&temporal_unit, VideoCodec::Av1));
        assert!(video_unit_is_recovery_point(
            &temporal_unit,
            VideoCodec::Av1
        ));
        assert!(!video_unit_is_keyframe(&obu(6, &[0x30]), VideoCodec::Av1));

        for codec in [VideoCodec::Jpeg, VideoCodec::Vp9] {
            assert!(!video_unit_is_keyframe(&avc, codec));
            assert!(!video_unit_is_recovery_point(&avc, codec));
            assert_eq!(NalCodec::from_video_codec(codec), None);
        }
        assert_eq!(NalCodec::from_video_codec(VideoCodec::Av1), None);
        assert_eq!(
            NalCodec::from_video_codec(VideoCodec::H265),
            Some(NalCodec::H265)
        );
    }

    #[test]
    fn parameter_sets_are_found_and_prepended() {
        let (sps, pps, idr) = (h264(3, 7), h264(3, 8), h264(3, 5));
        let stream = annex_b(&[&sps, &h264(0, 6), &pps, &idr]);
        assert_eq!(
            annex_b_parameter_sets(&stream, NalCodec::H264),
            vec![&sps[..], &pps[..]]
        );
        // A bare unit with no start code at all.
        assert_eq!(annex_b_parameter_sets(&sps, NalCodec::H264), vec![&sps[..]]);
        let (vps, hsps, hpps) = (h265(32, 0, 1), h265(33, 0, 1), h265(34, 0, 1));
        assert_eq!(
            annex_b_parameter_sets(
                &annex_b(&[&vps, &hsps, &hpps, &h265(19, 0, 1)]),
                NalCodec::H265
            ),
            vec![&vps[..], &hsps[..], &hpps[..]]
        );

        let mut unit = annex_b(&[&idr]);
        prepend_annex_b_units(&[sps.clone(), pps.clone()], &mut unit);
        assert_eq!(unit, annex_b(&[&sps, &pps, &idr]));
        assert!(access_unit_is_recovery_point(&unit, NalCodec::H264));
        let none: [&[u8]; 0] = [];
        prepend_annex_b_units(&none, &mut unit);
        assert_eq!(unit, annex_b(&[&sps, &pps, &idr]));
    }

    #[test]
    fn avc_decoder_configuration_records_are_read_strictly() {
        let record = [
            0x01, // configurationVersion
            0x42, 0x00, 0x1F, // profile, compatibility, level
            0xFF, // lengthSizeMinusOne = 3
            0xE1, // one SPS
            0x00, 0x03, 0x67, 0x42, 0x1F, // SPS
            0x01, // one PPS
            0x00, 0x02, 0x68, 0xCE, // PPS
        ];
        assert_eq!(
            parse_avc_decoder_config(&record),
            Some(vec![&[0x67, 0x42, 0x1F][..], &[0x68, 0xCE][..]])
        );
        // A High-profile extension after the PPS is ignored.
        let mut extended = record.to_vec();
        extended.extend([0xFD, 0xF8, 0xF8, 0x00]);
        assert!(parse_avc_decoder_config(&extended).is_some());

        // Wrong version, truncation anywhere, or half a configuration.
        let mut wrong_version = record;
        wrong_version[0] = 2;
        assert_eq!(parse_avc_decoder_config(&wrong_version), None);
        for cut in 0..record.len() {
            assert_eq!(parse_avc_decoder_config(&record[..cut]), None, "cut {cut}");
        }
        let mut no_pps = record[..11].to_vec();
        no_pps.push(0x00);
        assert_eq!(parse_avc_decoder_config(&no_pps), None);
        // A PPS where the SPS should be.
        let mut swapped = record;
        swapped[8] = 0x68;
        assert_eq!(parse_avc_decoder_config(&swapped), None);
    }
}
