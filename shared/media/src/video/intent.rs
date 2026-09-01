use std::fmt::{Display, Formatter};

use arcen_protocol::messages::{
    ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings, VideoSelectionIntent,
};

use crate::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, EncodeIntent,
    TransferCharacteristics, VideoCodec, VideoConfiguration,
};

use super::{VideoVariant, adaptive_codec_ladder};

/// Typed auth-time client request after wire-token and decode-capability
/// validation, but before a host applies its own ceiling or probes encoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedClientVideoRequest {
    pub selection: VideoSelectionIntent,
    pub video: VideoConfiguration,
    pub encode_intent: EncodeIntent,
    pub max_fps: u32,
    pub capabilities: ClientVideoCapabilitiesMsg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientVideoRequestError(String);

impl Display for ClientVideoRequestError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ClientVideoRequestError {}

fn invalid(message: impl Into<String>) -> ClientVideoRequestError {
    ClientVideoRequestError(message.into())
}

fn validate_quality(quality: &QualitySettings) -> Result<(), ClientVideoRequestError> {
    if quality.msg_type != "quality_settings" {
        return Err(invalid(
            "initial video request has the wrong nested message type",
        ));
    }
    for (name, token) in [
        ("codec", quality.codec.as_str()),
        ("chroma", quality.chroma.as_str()),
        ("bit_depth", quality.bit_depth.as_str()),
        ("color_range", quality.color_range.as_str()),
        ("color_matrix", quality.color_matrix.as_str()),
        ("encode_intent", quality.encode_intent.as_str()),
    ] {
        if token.is_empty()
            || token.len() > 32
            || !token.is_ascii()
            || token.chars().any(|character| character.is_ascii_control())
        {
            return Err(invalid(format!(
                "initial video request {name} token is invalid"
            )));
        }
    }
    if quality.max_fps == 0 || quality.max_fps > 240 {
        return Err(invalid(
            "initial video request max_fps must be between 1 and 240",
        ));
    }
    Ok(())
}

fn resolve_codec(
    quality: &QualitySettings,
    capabilities: ClientVideoCapabilitiesMsg,
) -> Result<VideoCodec, ClientVideoRequestError> {
    if quality.video_selection == VideoSelectionIntent::AdaptivePerformance {
        return adaptive_codec_ladder(VideoCodec::Av1)
            .iter()
            .copied()
            .find(|codec| match codec {
                VideoCodec::Av1 => capabilities.av1,
                VideoCodec::H265 => capabilities.h265,
                VideoCodec::H264 => capabilities.h264,
                VideoCodec::Jpeg | VideoCodec::Vp9 => false,
            })
            .ok_or_else(|| {
                invalid("initial video request has no decodable AV1, HEVC, or H.264 codec")
            });
    }
    let codec = VideoCodec::from_token(&quality.codec)
        .ok_or_else(|| invalid(format!("unsupported codec {:?}", quality.codec)))?;
    let supported = match codec {
        VideoCodec::Av1 => capabilities.av1,
        VideoCodec::H265 => capabilities.h265,
        VideoCodec::H264 => capabilities.h264,
        VideoCodec::Jpeg | VideoCodec::Vp9 => false,
    };
    if supported {
        Ok(codec)
    } else {
        Err(invalid(format!(
            "initial video request selects {}, which the client did not advertise",
            codec.token()
        )))
    }
}

fn resolve_video(
    request: &InitialVideoRequestMsg,
) -> Result<VideoConfiguration, ClientVideoRequestError> {
    let quality = &request.quality;
    let capabilities = request.capabilities;
    let chroma = ChromaSubsampling::from_token(&quality.chroma)
        .ok_or_else(|| invalid(format!("unsupported chroma {:?}", quality.chroma)))?;
    if quality.video_selection == VideoSelectionIntent::AdaptivePerformance
        && chroma != ChromaSubsampling::Yuv420
    {
        return Err(invalid(
            "adaptive performance requires yuv420; use color_fidelity for 4:4:4",
        ));
    }
    if chroma == ChromaSubsampling::Yuv444 && !capabilities.yuv444 {
        return Err(invalid(
            "initial video request selects yuv444 without client decode support",
        ));
    }
    let bit_depth = BitDepth::from_token(&quality.bit_depth)
        .ok_or_else(|| invalid(format!("unsupported bit depth {:?}", quality.bit_depth)))?;
    if bit_depth >= BitDepth::Ten && !capabilities.main10 {
        return Err(invalid(
            "initial video request selects 10/12-bit without client Main10 support",
        ));
    }
    if bit_depth == BitDepth::Twelve && !capabilities.main12 {
        return Err(invalid(
            "initial video request selects 12-bit without client Main12 support",
        ));
    }
    let range = ColorRange::from_token(&quality.color_range).ok_or_else(|| {
        invalid(format!(
            "unsupported colour range {:?}",
            quality.color_range
        ))
    })?;
    if range == ColorRange::Full && !capabilities.full_range {
        return Err(invalid(
            "initial video request selects full range without client support",
        ));
    }
    let matrix = ColorMatrix::from_token(&quality.color_matrix).ok_or_else(|| {
        invalid(format!(
            "unsupported colour matrix {:?}",
            quality.color_matrix
        ))
    })?;
    if matrix.is_identity() && !capabilities.identity_matrix {
        return Err(invalid(
            "initial video request selects identity matrix without client support",
        ));
    }
    if matrix == ColorMatrix::Bt601 && !capabilities.bt601_matrix {
        return Err(invalid(
            "initial video request selects BT.601 matrix without client support",
        ));
    }
    if matrix == ColorMatrix::Bt2020Ncl && !capabilities.bt2020_ncl_matrix {
        return Err(invalid(
            "initial video request selects BT.2020 NCL matrix without client support",
        ));
    }
    // Read from the request rather than pinned to BT.709. These two axes are
    // how a Deck asks for HDR, and discarding them here meant a PQ/BT.2020
    // request arrived at the host as BT.709 -- the ask never survived its own
    // parser, so no amount of host work downstream could have honoured it.
    //
    // An unknown token is an error rather than a silent BT.709: a client
    // naming a transfer this build does not know must not be told it was
    // served, which is the same rule the other colour axes already follow.
    let primaries = ColorPrimaries::from_token(&quality.color_primaries).ok_or_else(|| {
        invalid(format!(
            "unsupported colour primaries {:?}",
            quality.color_primaries
        ))
    })?;
    let transfer = TransferCharacteristics::from_token(&quality.transfer)
        .ok_or_else(|| invalid(format!("unsupported transfer {:?}", quality.transfer)))?;
    Ok(VideoConfiguration {
        codec: resolve_codec(quality, capabilities)?,
        chroma,
        bit_depth,
        range,
        matrix,
        primaries,
        transfer,
    })
}

/// Validate an auth-time video request and resolve only the client-owned side
/// of codec selection.
///
/// Adaptive performance ranks AV1, HEVC, then H.264 among codecs the client
/// claims it can decode. Actual host support remains unknown here and is
/// resolved by the platform's real encoder probe. Colour-fidelity and exact
/// requests preserve their concrete codec.
///
/// # Errors
///
/// Returns a bounded validation error when a token, frame-rate limit,
/// capability claim, or combined video contract is invalid.
pub fn resolve_client_video_request(
    request: &InitialVideoRequestMsg,
) -> Result<ResolvedClientVideoRequest, ClientVideoRequestError> {
    let quality = &request.quality;
    validate_quality(quality)?;
    let video = resolve_video(request)?;
    let encode_intent = EncodeIntent::from_token(&quality.encode_intent).ok_or_else(|| {
        invalid(format!(
            "unsupported encode intent {:?}",
            quality.encode_intent
        ))
    })?;
    let variant = VideoVariant::new(video);
    if !variant.is_coherent() {
        return Err(invalid(format!(
            "initial video request is an incoherent contract: {}",
            variant.id()
        )));
    }
    Ok(ResolvedClientVideoRequest {
        selection: quality.video_selection,
        video,
        encode_intent,
        max_fps: quality.max_fps,
        capabilities: request.capabilities,
    })
}

#[cfg(test)]
mod tests {
    use arcen_protocol::messages::{
        ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings,
    };

    use super::*;

    fn request(selection: VideoSelectionIntent) -> InitialVideoRequestMsg {
        InitialVideoRequestMsg {
            quality: QualitySettings {
                codec: "h264".to_string(),
                chroma: "yuv420".to_string(),
                bit_depth: "8".to_string(),
                color_range: "limited".to_string(),
                color_matrix: "bt709".to_string(),
                video_selection: selection,
                ..QualitySettings::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                av1: true,
                yuv444: true,
                main10: true,
                full_range: true,
                ..ClientVideoCapabilitiesMsg::default()
            },
        }
    }

    #[test]
    fn adaptive_ranks_client_decoders_without_changing_colour() {
        let mut request = request(VideoSelectionIntent::AdaptivePerformance);
        let resolved = resolve_client_video_request(&request).unwrap();
        assert_eq!(resolved.video.codec, VideoCodec::Av1);
        assert_eq!(resolved.video.chroma, ChromaSubsampling::Yuv420);
        assert_eq!(resolved.video.bit_depth, BitDepth::Eight);
        assert_eq!(resolved.video.range, ColorRange::Limited);
        assert_eq!(resolved.encode_intent, EncodeIntent::Interactive);

        request.capabilities.av1 = false;
        assert_eq!(
            resolve_client_video_request(&request).unwrap().video.codec,
            VideoCodec::H265
        );
        request.capabilities.h265 = false;
        assert_eq!(
            resolve_client_video_request(&request).unwrap().video.codec,
            VideoCodec::H264
        );

        request.quality.encode_intent = "quality".to_string();
        assert_eq!(
            resolve_client_video_request(&request)
                .unwrap()
                .encode_intent,
            EncodeIntent::Quality
        );
    }

    #[test]
    fn adaptive_refuses_444_and_exact_refuses_unadvertised_codec() {
        let mut adaptive = request(VideoSelectionIntent::AdaptivePerformance);
        adaptive.quality.chroma = "yuv444".to_string();
        assert!(resolve_client_video_request(&adaptive).is_err());

        let mut exact = request(VideoSelectionIntent::Exact);
        exact.quality.codec = "av1".to_string();
        exact.capabilities.av1 = false;
        assert!(resolve_client_video_request(&exact).is_err());

        let mut unknown_intent = request(VideoSelectionIntent::Exact);
        unknown_intent.quality.encode_intent = "best-effort".to_string();
        assert!(resolve_client_video_request(&unknown_intent).is_err());
    }
}
