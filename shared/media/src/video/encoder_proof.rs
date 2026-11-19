// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Admission proof for a hardware encoder a Pier cannot vouch for in advance.
//!
//! A driver that registers an encoder is not proof that it encodes what Arcen
//! asks for. A Media Foundation hardware MFT may accept a media type and still
//! emit High profile, 4:4:4 or a different size, or emit nothing at all. So a
//! Pier admits such an encoder only after one real encode whose sequence
//! parameter set, read by the shared bitstream parsers, states the codec,
//! chroma, depth, colour and picture size the session would claim.
//!
//! The capenc child prints the proof as one `[capenc] ADMITTED` line; the
//! Pier parses it here, so the format has one owner.

use std::fmt::{Display, Formatter};

use crate::bitstream::picture::{StreamCodecKind, StreamFormat};
use crate::bitstream::{h264, hevc, unescape_rbsp};
use crate::{VideoCodec, VideoConfiguration};

use super::EncoderBackend;

const ADMITTED_PREFIX: &str = "[capenc] ADMITTED ";
const ADMITTED_VERSION: &str = "1";
/// Longest encoder name carried in an admission line. Vendor MFT friendly
/// names are about 40 characters.
pub const MAX_ENCODER_NAME_CHARS: usize = 96;

/// Why an encoded access unit does not prove the encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderProofError {
    /// The codec has no parameter-set proof here (only H.264 and HEVC do).
    UnsupportedCodec(VideoCodec),
    /// The access unit carries no sequence parameter set.
    NoSequenceParameterSet,
    /// The sequence parameter set does not parse.
    Unparsable(String),
    /// The stream's profile is not one every Deck decodes for this codec.
    UnexpectedProfile { codec: VideoCodec, profile_idc: u8 },
    /// The stream contradicts the claimed codec, chroma, depth or colour.
    Contradiction(String),
    /// The decoder would output a different picture size.
    WrongSize {
        expected: (u32, u32),
        stream: (u32, u32),
    },
}

impl Display for EncoderProofError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedCodec(codec) => {
                write!(formatter, "no parameter-set proof for {}", codec.token())
            }
            Self::NoSequenceParameterSet => {
                formatter.write_str("the encoded access unit has no sequence parameter set")
            }
            Self::Unparsable(error) => {
                write!(
                    formatter,
                    "the sequence parameter set does not parse: {error}"
                )
            }
            Self::UnexpectedProfile { codec, profile_idc } => write!(
                formatter,
                "the stream is {} profile_idc {profile_idc}, which is not admitted",
                codec.token()
            ),
            Self::Contradiction(detail) => write!(formatter, "{detail}"),
            Self::WrongSize { expected, stream } => write!(
                formatter,
                "the stream decodes to {}x{} but {}x{} was asked for",
                stream.0, stream.1, expected.0, expected.1
            ),
        }
    }
}

impl std::error::Error for EncoderProofError {}

/// Whether a coded profile is one a hardware encoder may be admitted with:
/// H.264 Constrained Baseline, Main or High (every Deck decodes all three),
/// HEVC Main. Ten-bit and range-extension profiles are not admitted here
/// because the contracts that need them are not served by these encoders.
#[must_use]
pub const fn admitted_profile(codec: VideoCodec, profile_idc: u8) -> bool {
    match codec {
        VideoCodec::H264 => matches!(profile_idc, 66 | 77 | 100),
        VideoCodec::H265 => profile_idc == 1,
        VideoCodec::Av1 | VideoCodec::Jpeg | VideoCodec::Vp9 => false,
    }
}

/// The stream format stated by the first sequence parameter set in an
/// Annex B access unit of `codec`.
///
/// # Errors
///
/// [`EncoderProofError`] when the codec has no SPS, the access unit carries
/// none, or it does not parse.
pub fn sequence_format(
    codec: VideoCodec,
    access_unit: &[u8],
) -> Result<StreamFormat, EncoderProofError> {
    let mut found: Option<Result<StreamFormat, EncoderProofError>> = None;
    match codec {
        VideoCodec::H264 => crate::annexb::for_each_annex_b(access_unit, |nal| {
            if found.is_none() && nal.len() > 1 && nal[0] & 0x1F == 7 {
                found = Some(
                    h264::Sps::parse(&unescape_rbsp(&nal[1..]))
                        .map(|sps| h264::stream_format(&sps))
                        .map_err(|error| EncoderProofError::Unparsable(error.to_string())),
                );
            }
        }),
        VideoCodec::H265 => crate::annexb::for_each_annex_b(access_unit, |nal| {
            if found.is_none() && nal.len() > 2 && (nal[0] >> 1) & 0x3F == 33 {
                found = Some(
                    hevc::Sps::parse(&unescape_rbsp(&nal[2..]))
                        .map(|sps| hevc::stream_format(&sps))
                        .map_err(|error| EncoderProofError::Unparsable(error.to_string())),
                );
            }
        }),
        other @ (VideoCodec::Av1 | VideoCodec::Jpeg | VideoCodec::Vp9) => {
            return Err(EncoderProofError::UnsupportedCodec(other));
        }
    }
    found.unwrap_or(Err(EncoderProofError::NoSequenceParameterSet))
}

/// Proves that `access_unit` is the stream `claimed` at `width`x`height`
/// describes: an admitted profile, no contradiction in codec, chroma, depth
/// or the colour its VUI states, and the exact output picture size.
///
/// # Errors
///
/// [`EncoderProofError`] naming the first thing the stream does not prove.
pub fn prove_encoded_access_unit(
    claimed: &VideoConfiguration,
    width: u32,
    height: u32,
    access_unit: &[u8],
) -> Result<StreamFormat, EncoderProofError> {
    let format = sequence_format(claimed.codec, access_unit)?;
    let codec = match format.codec {
        StreamCodecKind::H264 => VideoCodec::H264,
        StreamCodecKind::H265 => VideoCodec::H265,
    };
    if !admitted_profile(codec, format.profile_idc) {
        return Err(EncoderProofError::UnexpectedProfile {
            codec,
            profile_idc: format.profile_idc,
        });
    }
    if let Some(detail) = format.contradiction(claimed) {
        return Err(EncoderProofError::Contradiction(detail));
    }
    let stream = (format.visible.2, format.visible.3);
    if stream != (width, height) {
        return Err(EncoderProofError::WrongSize {
            expected: (width, height),
            stream,
        });
    }
    Ok(format)
}

/// One proven encode, as the capenc child reports it to its Pier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareEncoderAdmission {
    pub backend: EncoderBackend,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// The profile the stream's own SPS states.
    pub profile_idc: u8,
    /// The encoder's own name (an MFT friendly name), for logs and diagnose.
    pub encoder_name: String,
}

impl HardwareEncoderAdmission {
    /// The admission line. The encoder name is last and runs to the end of
    /// the line, so it may contain spaces; control characters are dropped
    /// and it is cut at [`MAX_ENCODER_NAME_CHARS`].
    #[must_use]
    pub fn format_line(&self) -> String {
        let name: String = self
            .encoder_name
            .chars()
            .filter(|character| !character.is_control())
            .take(MAX_ENCODER_NAME_CHARS)
            .collect();
        format!(
            "{ADMITTED_PREFIX}version={ADMITTED_VERSION} backend={} codec={} width={} height={} profile={} encoder={}",
            self.backend.ready_token(),
            self.codec.token(),
            self.width,
            self.height,
            self.profile_idc,
            name.trim()
        )
    }

    /// Parses an admission line; anything else, or a line from another
    /// version, is `None`.
    #[must_use]
    pub fn parse_line(line: &str) -> Option<Self> {
        let rest = line
            .trim_end_matches(['\r', '\n'])
            .strip_prefix(ADMITTED_PREFIX)?;
        let (fields, encoder_name) = rest.split_once(" encoder=")?;
        if encoder_name.chars().any(char::is_control)
            || encoder_name.chars().count() > MAX_ENCODER_NAME_CHARS
        {
            return None;
        }
        let mut version = None;
        let mut backend = None;
        let mut codec = None;
        let mut width = None;
        let mut height = None;
        let mut profile = None;
        for field in fields.split(' ') {
            let (name, value) = field.split_once('=')?;
            let slot = match name {
                "version" => &mut version,
                "backend" => &mut backend,
                "codec" => &mut codec,
                "width" => &mut width,
                "height" => &mut height,
                "profile" => &mut profile,
                _ => return None,
            };
            if slot.replace(value).is_some() {
                return None;
            }
        }
        if version? != ADMITTED_VERSION {
            return None;
        }
        Some(Self {
            backend: EncoderBackend::from_token(backend?)?,
            codec: VideoCodec::from_token(codec?)?,
            width: width?.parse().ok().filter(|value| *value > 0)?,
            height: height?.parse().ok().filter(|value| *value > 0)?,
            profile_idc: profile?.parse().ok()?,
            encoder_name: encoder_name.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVENC_H264_HIGH: &[u8] =
        include_bytes!("../../tests/data/bitstream/nvenc-h264-high-640x360.264");
    const NVENC_HEVC_MAIN: &[u8] =
        include_bytes!("../../tests/data/bitstream/nvenc-hevc-main-640x360.265");
    const NVENC_HEVC_MAIN10: &[u8] =
        include_bytes!("../../tests/data/bitstream/nvenc-hevc-main10-640x360.265");

    fn sdr(codec: VideoCodec) -> VideoConfiguration {
        VideoConfiguration {
            codec,
            ..VideoConfiguration::legacy_h264()
        }
    }

    #[test]
    fn a_real_h264_and_hevc_main_stream_prove_their_claim() {
        let h264 = prove_encoded_access_unit(&sdr(VideoCodec::H264), 640, 360, NVENC_H264_HIGH)
            .expect("H.264 High 4:2:0 8-bit proves");
        assert_eq!(h264.profile_idc, 100);
        let hevc = prove_encoded_access_unit(&sdr(VideoCodec::H265), 640, 360, NVENC_HEVC_MAIN)
            .expect("HEVC Main proves");
        assert_eq!(hevc.profile_idc, 1);
    }

    #[test]
    fn a_stream_that_differs_from_the_claim_is_not_admitted() {
        assert!(matches!(
            prove_encoded_access_unit(&sdr(VideoCodec::H264), 1280, 720, NVENC_H264_HIGH),
            Err(EncoderProofError::WrongSize {
                expected: (1280, 720),
                stream: (640, 360)
            })
        ));
        // A Main10 stream answering an 8-bit request is refused on profile
        // before anything else.
        assert!(matches!(
            prove_encoded_access_unit(&sdr(VideoCodec::H265), 640, 360, NVENC_HEVC_MAIN10),
            Err(EncoderProofError::UnexpectedProfile { profile_idc: 2, .. })
        ));
        // An HEVC stream offered as H.264 has no H.264 SPS at all.
        assert_eq!(
            prove_encoded_access_unit(&sdr(VideoCodec::H264), 640, 360, NVENC_HEVC_MAIN),
            Err(EncoderProofError::NoSequenceParameterSet)
        );
        assert_eq!(
            prove_encoded_access_unit(&sdr(VideoCodec::Av1), 640, 360, NVENC_HEVC_MAIN),
            Err(EncoderProofError::UnsupportedCodec(VideoCodec::Av1))
        );
        assert_eq!(
            sequence_format(VideoCodec::H264, &[0, 0, 0, 1, 0x67]),
            Err(EncoderProofError::NoSequenceParameterSet)
        );
    }

    #[test]
    fn colour_the_vui_states_must_match_the_claim() {
        let full = VideoConfiguration {
            range: crate::ColorRange::Full,
            ..sdr(VideoCodec::H265)
        };
        let format = sequence_format(VideoCodec::H265, NVENC_HEVC_MAIN).expect("parses");
        let proved = prove_encoded_access_unit(&full, 640, 360, NVENC_HEVC_MAIN);
        if format.colour.is_some_and(|colour| !colour.full_range) {
            assert!(matches!(proved, Err(EncoderProofError::Contradiction(_))));
        } else {
            assert!(proved.is_ok());
        }
    }

    #[test]
    fn admission_line_round_trips_and_rejects_anything_else() {
        let admission = HardwareEncoderAdmission {
            backend: EncoderBackend::MediaFoundationHardware,
            codec: VideoCodec::H265,
            width: 1920,
            height: 1080,
            profile_idc: 1,
            encoder_name: "Intel\u{ae} Quick Sync Video H.265 Encoder MFT\n".to_string(),
        };
        let line = admission.format_line();
        assert_eq!(
            line,
            "[capenc] ADMITTED version=1 backend=media-foundation-hw codec=h265 width=1920 \
             height=1080 profile=1 encoder=Intel\u{ae} Quick Sync Video H.265 Encoder MFT"
        );
        let parsed = HardwareEncoderAdmission::parse_line(&format!("{line}\r\n")).expect("parses");
        assert_eq!(
            parsed.encoder_name,
            "Intel\u{ae} Quick Sync Video H.265 Encoder MFT"
        );
        assert_eq!(parsed.codec, VideoCodec::H265);
        assert_eq!(parsed.backend, EncoderBackend::MediaFoundationHardware);

        for bad in [
            line.replace("version=1", "version=2"),
            line.replace("codec=h265", "codec=h266"),
            line.replace("width=1920", "width=0"),
            line.replace("profile=1", "profile=1 profile=1"),
            line.replace("backend=", "colour=x backend="),
            line.replace(" encoder=", " name="),
            line.replace("[capenc] ADMITTED", "[capenc] READY"),
            format!("{line}\u{7}"),
        ] {
            assert_eq!(HardwareEncoderAdmission::parse_line(&bad), None, "{bad}");
        }
    }
}
