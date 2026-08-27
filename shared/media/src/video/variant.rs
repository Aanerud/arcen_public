//! The colour-variation matrix.
//!
//! Arcen's high-fidelity colour work resolves its uncertainties by measurement
//! rather than by design: several questions (does this Mac decode HEVC Rext at
//! ten bits in hardware? does an identity-matrix stream survive `CoreVideo`? is
//! 4:4:4 ten-bit fast enough at 4K60?) have no authoritative answer in any
//! vendor document, and guessing conservatively costs real quality.
//!
//! A [`VideoVariant`] is one row of that matrix: a complete, self-consistent
//! description of a coded video format. It has a stable string id used
//! identically by capenc argv, log lines, the Deck's variant picker, and the
//! committed results file, so a finding recorded on one machine names exactly
//! the same thing everywhere else.

use std::fmt::{Display, Formatter};

use crate::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
    VideoCodec, VideoConfiguration,
};

/// One row of the colour-variation matrix.
///
/// This is deliberately a thin wrapper over [`VideoConfiguration`] rather than
/// a parallel vocabulary: the matrix must exercise the same type the resolver,
/// the wire and the encoders use, or it would be testing something other than
/// the product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoVariant {
    /// The coded format this row describes.
    pub video: VideoConfiguration,
    /// Whether this row asks the encoder for a lossless mode.
    pub lossless: bool,
}

/// Failure to parse a variant id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantIdError {
    /// The id had too few or too many `-` separated parts.
    MalformedShape(String),
    /// A component was not a recognised token.
    UnknownComponent {
        component: &'static str,
        value: String,
    },
    /// The components parsed but do not describe a format Arcen offers.
    Incoherent(String),
}

impl Display for VariantIdError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedShape(id) => {
                write!(
                    formatter,
                    "variant id `{id}` is not codec-chroma-depth-range-matrix[-lossless]"
                )
            }
            Self::UnknownComponent { component, value } => {
                write!(formatter, "unknown {component} `{value}` in variant id")
            }
            Self::Incoherent(id) => {
                write!(
                    formatter,
                    "variant id `{id}` does not describe an offered format"
                )
            }
        }
    }
}

impl std::error::Error for VariantIdError {}

/// Short chroma token used inside a variant id.
///
/// Deliberately shorter than [`ChromaSubsampling::token`]: an id is typed by
/// hand into argv and read in log lines, so `hevc-444-10-full-bt709` beats
/// `h265-yuv444-10-full-bt709`.
const fn chroma_id(chroma: ChromaSubsampling) -> &'static str {
    match chroma {
        ChromaSubsampling::Yuv420 => "420",
        ChromaSubsampling::Yuv422 => "422",
        ChromaSubsampling::Yuv444 => "444",
    }
}

fn chroma_from_id(value: &str) -> Option<ChromaSubsampling> {
    match value {
        "420" => Some(ChromaSubsampling::Yuv420),
        "422" => Some(ChromaSubsampling::Yuv422),
        "444" => Some(ChromaSubsampling::Yuv444),
        _ => None,
    }
}

/// Short codec token used inside a variant id.
const fn codec_id(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Jpeg => "jpeg",
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "hevc",
        VideoCodec::Vp9 => "vp9",
        VideoCodec::Av1 => "av1",
    }
}

fn codec_from_id(value: &str) -> Option<VideoCodec> {
    match value {
        "jpeg" => Some(VideoCodec::Jpeg),
        "h264" => Some(VideoCodec::H264),
        // `hevc` is the name a colourist uses; `h265` is accepted so an id
        // copied out of a codec token still works.
        "hevc" | "h265" => Some(VideoCodec::H265),
        "vp9" => Some(VideoCodec::Vp9),
        "av1" => Some(VideoCodec::Av1),
        _ => None,
    }
}

impl VideoVariant {
    /// Build a variant from a coded format.
    #[must_use]
    pub const fn new(video: VideoConfiguration) -> Self {
        Self {
            video,
            lossless: false,
        }
    }

    /// This variant asking for a lossless encode.
    #[must_use]
    pub const fn lossless(mut self) -> Self {
        self.lossless = true;
        self
    }

    /// The stable id for this variant.
    ///
    /// Shape: `codec-chroma-depth-range-matrix` plus a `-lossless` suffix.
    /// Primaries and transfer are omitted because every row Arcen currently
    /// tests is BT.709; if an HDR row is ever added the id gains a component
    /// and old ids keep parsing, because parsing is by position with defaults.
    #[must_use]
    pub fn id(self) -> String {
        let base = format!(
            "{}-{}-{}-{}-{}",
            codec_id(self.video.codec),
            chroma_id(self.video.chroma),
            self.video.bit_depth.token(),
            self.video.range.token(),
            self.video.matrix.token(),
        );
        if self.lossless {
            format!("{base}-lossless")
        } else {
            base
        }
    }

    /// Parse a stable variant id.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for a malformed shape, an unknown component, or
    /// a combination Arcen does not offer for that codec.
    pub fn from_id(id: &str) -> Result<Self, VariantIdError> {
        let mut parts = id.split('-');
        let codec = parts.next().unwrap_or_default();
        let chroma = parts
            .next()
            .ok_or_else(|| VariantIdError::MalformedShape(id.to_owned()))?;
        let depth = parts
            .next()
            .ok_or_else(|| VariantIdError::MalformedShape(id.to_owned()))?;
        let range = parts
            .next()
            .ok_or_else(|| VariantIdError::MalformedShape(id.to_owned()))?;
        let matrix = parts
            .next()
            .ok_or_else(|| VariantIdError::MalformedShape(id.to_owned()))?;
        let lossless = match parts.next() {
            None => false,
            Some("lossless") => true,
            Some(other) => {
                return Err(VariantIdError::UnknownComponent {
                    component: "suffix",
                    value: other.to_owned(),
                });
            }
        };
        if parts.next().is_some() {
            return Err(VariantIdError::MalformedShape(id.to_owned()));
        }

        let codec = codec_from_id(codec).ok_or_else(|| VariantIdError::UnknownComponent {
            component: "codec",
            value: codec.to_owned(),
        })?;
        let chroma = chroma_from_id(chroma).ok_or_else(|| VariantIdError::UnknownComponent {
            component: "chroma",
            value: chroma.to_owned(),
        })?;
        let bit_depth =
            BitDepth::from_token(depth).ok_or_else(|| VariantIdError::UnknownComponent {
                component: "bit depth",
                value: depth.to_owned(),
            })?;
        let range =
            ColorRange::from_token(range).ok_or_else(|| VariantIdError::UnknownComponent {
                component: "colour range",
                value: range.to_owned(),
            })?;
        let matrix =
            ColorMatrix::from_token(matrix).ok_or_else(|| VariantIdError::UnknownComponent {
                component: "matrix",
                value: matrix.to_owned(),
            })?;

        let variant = Self {
            video: VideoConfiguration {
                codec,
                chroma,
                bit_depth,
                range,
                matrix,
                primaries: ColorPrimaries::Bt709,
                transfer: TransferCharacteristics::Bt709,
            },
            lossless,
        };
        if !variant.is_coherent() {
            return Err(VariantIdError::Incoherent(id.to_owned()));
        }
        Ok(variant)
    }

    /// Whether this variant describes a format Arcen offers for its codec.
    ///
    /// This checks the product's own offer tables, not any backend: a variant
    /// can be perfectly coherent and still be unavailable on a given host, and
    /// those are different answers that the matrix reports separately.
    #[must_use]
    pub fn is_coherent(self) -> bool {
        if !self
            .video
            .codec
            .offered_chroma()
            .contains(self.video.chroma)
        {
            return false;
        }
        if !self
            .video
            .codec
            .offered_bit_depths()
            .contains(self.video.bit_depth)
        {
            return false;
        }
        // An identity matrix means the coded planes carry G, B and R at full
        // resolution. Subsampling them would discard two thirds of the red and
        // blue channels, which is not a thing anyone means by "identity".
        if self.video.matrix.is_identity() && self.video.chroma != ChromaSubsampling::Yuv444 {
            return false;
        }
        true
    }

    /// A human-readable one-line description for logs and the Deck UI.
    #[must_use]
    pub fn describe(self) -> String {
        let matrix = if self.video.matrix.is_identity() {
            " GBR".to_string()
        } else {
            String::new()
        };
        let lossless = if self.lossless { " lossless" } else { "" };
        format!(
            "{} {} {}-bit {}{matrix}{lossless}",
            match self.video.codec {
                VideoCodec::H265 => "HEVC",
                VideoCodec::H264 => "H.264",
                VideoCodec::Av1 => "AV1",
                VideoCodec::Vp9 => "VP9",
                VideoCodec::Jpeg => "JPEG",
            },
            match self.video.chroma {
                ChromaSubsampling::Yuv420 => "4:2:0",
                ChromaSubsampling::Yuv422 => "4:2:2",
                ChromaSubsampling::Yuv444 => "4:4:4",
            },
            self.video.bit_depth.bits(),
            match self.video.range {
                ColorRange::Full => "full range",
                ColorRange::Limited => "limited range",
            },
        )
    }
}

/// Build a variant from parts, for the const matrix table below.
const fn variant(
    codec: VideoCodec,
    chroma: ChromaSubsampling,
    bit_depth: BitDepth,
    range: ColorRange,
    matrix: ColorMatrix,
) -> VideoVariant {
    VideoVariant {
        video: VideoConfiguration {
            codec,
            chroma,
            bit_depth,
            range,
            matrix,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        },
        lossless: false,
    }
}

/// The rows Arcen actually probes, in the order they should be reported.
///
/// This deliberately includes rows that are expected to fail. A row that fails
/// is a recorded finding; a row that was never attempted is an assumption, and
/// assumptions are what this matrix exists to eliminate. In particular
/// `h264-444-8-*` is here because no Apple document states whether `VideoToolbox`
/// decodes High 4:4:4 Predictive at all, and the identity rows are here because
/// `CoreVideo` has no identity matrix constant and the practical consequence of
/// that is unknown until measured.
pub const PROBE_MATRIX: &[VideoVariant] = &[
    // Control. This is what Arcen ships today, and it must keep working.
    variant(
        VideoCodec::H265,
        ChromaSubsampling::Yuv444,
        BitDepth::Eight,
        ColorRange::Limited,
        ColorMatrix::Bt709,
    ),
    // Isolates the range change from the depth change.
    variant(
        VideoCodec::H265,
        ChromaSubsampling::Yuv444,
        BitDepth::Eight,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // The target format.
    variant(
        VideoCodec::H265,
        ChromaSubsampling::Yuv444,
        BitDepth::Ten,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // Isolates range from depth at ten bits.
    variant(
        VideoCodec::H265,
        ChromaSubsampling::Yuv444,
        BitDepth::Ten,
        ColorRange::Limited,
        ColorMatrix::Bt709,
    ),
    // Fallback if Rext ten-bit turns out not to decode: Main 10 is the one
    // ten-bit profile Apple documents.
    variant(
        VideoCodec::H265,
        ChromaSubsampling::Yuv420,
        BitDepth::Ten,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // Cheapest possible full-range win, for the widest possible client set.
    variant(
        VideoCodec::H264,
        ChromaSubsampling::Yuv420,
        BitDepth::Eight,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // Does `VideoToolbox` decode High 4:4:4 Predictive at all? Undocumented.
    variant(
        VideoCodec::H264,
        ChromaSubsampling::Yuv444,
        BitDepth::Eight,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // Blackwell-only on the encode side; free to attempt everywhere else.
    // These rows are expected to report `unsupported` on pre-Blackwell silicon
    // *and* on Blackwell until the NVENC bindings move to Video Codec SDK 13.0
    // — 12.1 has no 4:2:2 surface format to name. They stay in the matrix so
    // the Deck's decode side is exercised and the day the bindings land the
    // answer is one probe run away. See `docs/architecture/nvenc-sdk13-blackwell.md`.
    variant(
        VideoCodec::H265,
        ChromaSubsampling::Yuv422,
        BitDepth::Ten,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    variant(
        VideoCodec::H265,
        ChromaSubsampling::Yuv422,
        BitDepth::Eight,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // H.264 above eight bits is likewise Blackwell-only. NVENC rejects it on
    // every earlier generation regardless of bindings.
    variant(
        VideoCodec::H264,
        ChromaSubsampling::Yuv420,
        BitDepth::Ten,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // Settles the RGB-identity question empirically.
    variant(
        VideoCodec::H265,
        ChromaSubsampling::Yuv444,
        BitDepth::Ten,
        ColorRange::Full,
        ColorMatrix::Identity,
    ),
    // Hardware AV1 on both ends: NVENC encodes AV1 Main (4:2:0 8-bit) from
    // Ada onward, and Apple hardware-decodes AV1 from M3 onward. This row
    // answers whether the mainline 4:2:0 delivery tier can move off the
    // H.264/HEVC patent-pool royalties entirely onto the royalty-free
    // AOMedia Patent License -- the question this whole matrix addition
    // exists to settle.
    variant(
        VideoCodec::Av1,
        ChromaSubsampling::Yuv420,
        BitDepth::Eight,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // Same hardware-both-ends royalty question at ten-bit: NVENC's AV1 Main
    // 10 profile is still 4:2:0, and the same M3-and-later Apple decoders
    // handle it, so this isolates whether the royalty-free path survives
    // once HDR-capable depth is requested.
    variant(
        VideoCodec::Av1,
        ChromaSubsampling::Yuv420,
        BitDepth::Ten,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    // Software tier, and the only twelve-bit route that exists anywhere.
    variant(
        VideoCodec::Av1,
        ChromaSubsampling::Yuv444,
        BitDepth::Ten,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
    variant(
        VideoCodec::Av1,
        ChromaSubsampling::Yuv444,
        BitDepth::Twelve,
        ColorRange::Full,
        ColorMatrix::Bt709,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_for_every_probe_row() {
        for expected in PROBE_MATRIX.iter().copied() {
            let id = expected.id();
            let parsed = VideoVariant::from_id(&id)
                .unwrap_or_else(|error| panic!("row `{id}` must parse: {error}"));
            assert_eq!(parsed, expected, "row `{id}` did not round trip");
        }
    }

    #[test]
    fn probe_ids_are_unique() {
        let mut ids: Vec<String> = PROBE_MATRIX.iter().copied().map(VideoVariant::id).collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "two probe rows share an id");
    }

    #[test]
    fn every_probe_row_is_coherent() {
        // A row that Arcen does not even offer could never produce a
        // meaningful finding, so an incoherent row is a bug in the table
        // rather than an interesting negative result.
        for row in PROBE_MATRIX.iter().copied() {
            assert!(row.is_coherent(), "row `{}` is not offered", row.id());
        }
    }

    #[test]
    fn the_target_format_is_present_and_named_as_documented() {
        let target = VideoVariant::from_id("hevc-444-10-full-bt709").expect("target row parses");
        assert!(PROBE_MATRIX.contains(&target));
        assert_eq!(target.video, VideoConfiguration::grading_reference());
    }

    #[test]
    fn hardware_both_ends_av1_rows_are_present_and_coherent() {
        // These two rows are the ones that can answer the royalty question:
        // NVENC AV1 (Ada+) and Apple hardware AV1 decode (M3+) both land only
        // on 4:2:0, so this is the tier that could move off the H.264/HEVC
        // patent-pool royalties onto the royalty-free AOMedia license.
        for id in ["av1-420-8-full-bt709", "av1-420-10-full-bt709"] {
            let row = VideoVariant::from_id(id)
                .unwrap_or_else(|error| panic!("`{id}` must be coherent: {error}"));
            assert!(
                PROBE_MATRIX.contains(&row),
                "`{id}` must be in PROBE_MATRIX"
            );
            assert_eq!(row.video.codec, VideoCodec::Av1);
            assert_eq!(row.video.chroma, ChromaSubsampling::Yuv420);
            assert_eq!(row.video.range, ColorRange::Full);
            assert_eq!(row.video.matrix, ColorMatrix::Bt709);
        }
    }

    #[test]
    fn identity_matrix_below_444_is_rejected_as_incoherent() {
        // Subsampling GBR would discard three quarters of the red and blue
        // channels, which is not what anyone means by an identity matrix.
        assert_eq!(
            VideoVariant::from_id("hevc-420-10-full-identity"),
            Err(VariantIdError::Incoherent(
                "hevc-420-10-full-identity".to_owned()
            ))
        );
    }

    #[test]
    fn twelve_bit_is_offered_only_where_a_path_exists() {
        // AV1 has a software twelve-bit path; HEVC does not, because NVENC
        // has no twelve-bit mode at all and no software HEVC ships here.
        assert!(VideoVariant::from_id("av1-444-12-full-bt709").is_ok());
        assert_eq!(
            VideoVariant::from_id("hevc-444-12-full-bt709"),
            Err(VariantIdError::Incoherent(
                "hevc-444-12-full-bt709".to_owned()
            ))
        );
    }

    #[test]
    fn malformed_and_unknown_ids_are_typed_failures_not_silent_defaults() {
        assert!(matches!(
            VideoVariant::from_id("hevc-444-10"),
            Err(VariantIdError::MalformedShape(_))
        ));
        // A trailing component that is not the lossless suffix names a
        // specific thing that is wrong, so it reports as an unknown suffix
        // rather than a vague shape complaint.
        assert!(matches!(
            VideoVariant::from_id("hevc-444-10-full-bt709-extra"),
            Err(VariantIdError::UnknownComponent {
                component: "suffix",
                ..
            })
        ));
        assert!(matches!(
            VideoVariant::from_id("hevc-444-10-full-bt709-lossless-more"),
            Err(VariantIdError::MalformedShape(_))
        ));
        assert!(matches!(
            VideoVariant::from_id("vvc-444-10-full-bt709"),
            Err(VariantIdError::UnknownComponent {
                component: "codec",
                ..
            })
        ));
        assert!(matches!(
            VideoVariant::from_id("hevc-444-9-full-bt709"),
            Err(VariantIdError::UnknownComponent {
                component: "bit depth",
                ..
            })
        ));
    }

    #[test]
    fn lossless_suffix_round_trips() {
        let lossless = VideoVariant::new(VideoConfiguration::grading_reference()).lossless();
        assert_eq!(lossless.id(), "hevc-444-10-full-bt709-lossless");
        assert_eq!(
            VideoVariant::from_id("hevc-444-10-full-bt709-lossless"),
            Ok(lossless)
        );
    }

    #[test]
    fn h265_id_alias_parses_but_is_not_the_canonical_spelling() {
        let canonical = VideoVariant::from_id("hevc-444-10-full-bt709").expect("canonical");
        let alias = VideoVariant::from_id("h265-444-10-full-bt709").expect("alias");
        assert_eq!(canonical, alias);
        assert_eq!(alias.id(), "hevc-444-10-full-bt709");
    }

    #[test]
    fn descriptions_are_human_readable() {
        assert_eq!(
            VideoVariant::new(VideoConfiguration::grading_reference()).describe(),
            "HEVC 4:4:4 10-bit full range"
        );
        assert_eq!(
            VideoVariant::new(VideoConfiguration::legacy_h264()).describe(),
            "H.264 4:2:0 8-bit limited range"
        );
    }
}
