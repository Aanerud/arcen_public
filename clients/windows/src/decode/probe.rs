// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The hello's decode capabilities: what the adapter proved, cached per
//! adapter for the process.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use arcen_deck_core::decode::{DecodeCapabilities, DecoderBackendFamily};
use arcen_deck_core::decode_report::{AV1_PROFILE, DecodeRefusal};
use windows::Win32::Foundation::LUID;

use arcen_media::annexb::{NalCodec, split_access_units};
use arcen_media::bitstream::picture::StreamCodecKind;

use super::backend::D3d11VideoDecoder;
use super::device::{
    DecoderProfile, H264, HEVC_MAIN, HEVC_MAIN10, HEVC_MAIN10_444, ProfileProbe, VideoDevice,
    hardware_adapters,
};
use super::readback::read_planar;

/// A real keyframe for one advertised profile: the first access unit of an
/// NVENC stream of Arcen's own test picture (640x360,
/// `shared/media/tests/data/bitstream/`), and the FNV-1a 64 of its planar
/// picture as an independent software decoder (ffmpeg, offline, never
/// shipped) reconstructs it.
struct ProbeKeyframe {
    profile: DecoderProfile,
    codec: StreamCodecKind,
    stream: &'static [u8],
    fnv: u64,
}

const PROBE_KEYFRAMES: [ProbeKeyframe; 4] = [
    ProbeKeyframe {
        profile: H264,
        codec: StreamCodecKind::H264,
        stream: include_bytes!(
            "../../../../shared/media/tests/data/bitstream/nvenc-h264-high-640x360.264"
        ),
        fnv: 0xd4b1_d3aa_1da3_8d2c,
    },
    ProbeKeyframe {
        profile: HEVC_MAIN,
        codec: StreamCodecKind::H265,
        stream: include_bytes!(
            "../../../../shared/media/tests/data/bitstream/nvenc-hevc-main-640x360.265"
        ),
        fnv: 0xacd3_8bc8_693a_2f7e,
    },
    ProbeKeyframe {
        profile: HEVC_MAIN10,
        codec: StreamCodecKind::H265,
        stream: include_bytes!(
            "../../../../shared/media/tests/data/bitstream/nvenc-hevc-main10-640x360.265"
        ),
        fnv: 0x8e60_8f36_456c_5081,
    },
    ProbeKeyframe {
        profile: HEVC_MAIN10_444,
        codec: StreamCodecKind::H265,
        stream: include_bytes!(
            "../../../../shared/media/tests/data/bitstream/nvenc-hevc-rext444-10-640x360.265"
        ),
        fnv: 0x9983_9e48_0c21_c0a0,
    },
];

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Decodes `profile`'s probe keyframe and checks every sample: a profile
/// that is listed and creates a decoder but reconstructs wrongly must not
/// be advertised.
///
/// # Errors
///
/// Why the keyframe did not decode exactly, or that `profile` has no probe
/// keyframe (and so cannot be advertised).
pub fn prove_keyframe(device: &VideoDevice, profile: &DecoderProfile) -> Result<(), String> {
    let keyframe = PROBE_KEYFRAMES
        .iter()
        .find(|keyframe| keyframe.profile == *profile)
        .ok_or_else(|| format!("{} has no probe keyframe", profile.name))?;
    let nal = match keyframe.codec {
        StreamCodecKind::H264 => NalCodec::H264,
        StreamCodecKind::H265 => NalCodec::H265,
    };
    let unit = split_access_units(nal, keyframe.stream)
        .into_iter()
        .next()
        .ok_or("probe stream has no access unit")?;
    let mut decoder = D3d11VideoDecoder::new(device.clone());
    let pictures = decoder
        .decode_annex_b(keyframe.codec, unit)
        .map_err(|error| format!("{} probe keyframe: {error}", profile.name))?;
    let picture = pictures
        .first()
        .ok_or_else(|| format!("{} probe keyframe produced no picture", profile.name))?;
    let planar = read_planar(&device.device, &device.context_lock, picture)?;
    if fnv1a(&planar) == keyframe.fnv {
        Ok(())
    } else {
        Err(format!(
            "{} probe keyframe decoded wrongly on {}",
            profile.name, device.adapter_name
        ))
    }
}

/// Every profile's probe on `device`, and the hello's capabilities. A
/// profile counts only after its decoder is created at the probe size
/// (3840x2160) *and* a real keyframe decodes sample-exact; profiles the
/// hello does not advertise (HEVC Main 4:4:4 eight-bit) keep their
/// creation result for the statistics.
#[must_use]
pub fn prove(device: &VideoDevice) -> (DecodeCapabilities, Vec<ProfileProbe>) {
    let (mut capabilities, mut results) = device.capabilities();
    for (profile, result) in &mut results {
        let advertised = PROBE_KEYFRAMES
            .iter()
            .any(|keyframe| keyframe.profile == *profile);
        if advertised && result.is_ok() {
            *result = prove_keyframe(device, profile);
        }
    }
    let proven = |wanted: &DecoderProfile| {
        results
            .iter()
            .any(|(profile, result)| profile == wanted && result.is_ok())
    };
    capabilities.h264 = proven(&H264);
    capabilities.h265 = proven(&HEVC_MAIN);
    capabilities.main10 = proven(&HEVC_MAIN10);
    capabilities.yuv444 = proven(&HEVC_MAIN10_444);
    (capabilities, results)
}

/// What one adapter proved: the hello's capabilities and, for every
/// profile it refused, the reason (shown when a contract is refused, for
/// example Grading on an adapter without HEVC 4:4:4 10).
#[derive(Debug, Clone, Default)]
pub struct ProbeReport {
    pub adapter: String,
    pub capabilities: DecodeCapabilities,
    pub refusals: Vec<(&'static str, String)>,
}

static CACHE: OnceLock<Mutex<HashMap<(u32, i32), ProbeReport>>> = OnceLock::new();

/// Nothing decodes: what a machine without a usable adapter advertises.
fn nothing() -> DecodeCapabilities {
    DecodeCapabilities {
        backend: DecoderBackendFamily::D3d11Video,
        ..DecodeCapabilities::default()
    }
}

/// Probes the adapter with `luid` (the one that owns the window's monitor),
/// once per process; later calls return the cached answer. An unknown LUID
/// advertises nothing rather than guessing.
#[must_use]
pub fn probe_for_adapter(luid: LUID) -> DecodeCapabilities {
    probe_report_for_adapter(luid).capabilities
}

/// [`probe_for_adapter`] with the adapter's name and the refusal reasons.
#[must_use]
pub fn probe_report_for_adapter(luid: LUID) -> ProbeReport {
    let key = (luid.LowPart, luid.HighPart);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache.lock().ok().and_then(|cache| cache.get(&key).cloned()) {
        return found;
    }
    let report = hardware_adapters()
        .ok()
        .and_then(|adapters| {
            adapters.into_iter().find(|(_, _, _, _, adapter_luid)| {
                adapter_luid.LowPart == luid.LowPart && adapter_luid.HighPart == luid.HighPart
            })
        })
        .and_then(|(adapter, name, vendor, device, _)| {
            VideoDevice::create(&adapter, name, vendor, device).ok()
        })
        .map_or_else(
            || ProbeReport {
                adapter: String::new(),
                capabilities: nothing(),
                refusals: vec![("adapter", "no video decoder on this adapter".to_owned())],
            },
            |device| {
                let (capabilities, results) = prove(&device);
                let mut refusals: Vec<(&'static str, String)> = results
                    .into_iter()
                    .filter_map(|(profile, result)| {
                        result.err().map(|reason| (profile.name, reason))
                    })
                    .collect();
                refusals.push((AV1_PROFILE, av1_refusal(&device).reason));
                ProbeReport {
                    adapter: device.adapter_name.clone(),
                    capabilities,
                    refusals,
                }
            },
        );
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, report.clone());
    }
    report
}

/// Why AV1 is not advertised on `device`, in the shared words: the GPU has
/// no AV1 decoder, or it has one and this Deck does not decode AV1 yet.
fn av1_refusal(device: &VideoDevice) -> DecodeRefusal {
    if device.lists_av1() {
        DecodeRefusal::not_decoded_yet(AV1_PROFILE, &device.adapter_name)
    } else {
        DecodeRefusal::not_offered(AV1_PROFILE, &device.adapter_name)
    }
}

/// Probes the first hardware adapter (the system's default), for callers
/// that do not know the window's adapter yet.
#[must_use]
pub fn probe_decode_capabilities() -> DecodeCapabilities {
    hardware_adapters()
        .ok()
        .and_then(|adapters| adapters.into_iter().next())
        .map_or_else(nothing, |(_, _, _, _, luid)| probe_for_adapter(luid))
}
