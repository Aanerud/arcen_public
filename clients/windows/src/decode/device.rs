// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Decoder profiles, devices and the per-adapter capability probe.
//!
//! What a Deck advertises is what the adapter proved: the profile is listed,
//! the output format is accepted, a configuration with the short slice
//! format exists, and a decoder is created at the advertised size. A probe
//! never assumes; a missing profile is a refusal with a reason.

use arcen_deck_core::decode::{DecodeCapabilities, DecoderBackendFamily, NativePictureFormat};
use arcen_media::bitstream::picture::{StreamCodecKind, StreamFormat};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
    D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, D3D11_SDK_VERSION,
    D3D11_VIDEO_DECODER_CONFIG, D3D11_VIDEO_DECODER_DESC, D3D11CreateDevice, ID3D11Device,
    ID3D11Multithread, ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_Y410,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIFactory1,
};
use windows::core::{GUID, Interface};

/// `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN_444` (Windows 11 24H2 SDK).
pub const HEVC_VLD_MAIN_444: GUID = GUID::from_u128(0x4008_018f_f537_4b36_98cf_61af_8a2c_1a33);
/// `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10_444` (Windows 11 24H2 SDK).
pub const HEVC_VLD_MAIN10_444: GUID = GUID::from_u128(0x0dab_effa_4458_4602_bc03_0795_659d_617c);
/// `DXVA_NoEncrypt`.
pub const DXVA_NO_ENCRYPT: GUID = GUID::from_u128(0x1b81_bed0_a0c7_11d3_b984_00c0_4f2e_73c5);

/// The size every profile is probed at: the largest picture a Pier sends a
/// single monitor at today.
pub const PROBE_WIDTH: u32 = 3840;
pub const PROBE_HEIGHT: u32 = 2160;

/// One DXVA decode profile Arcen uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderProfile {
    pub name: &'static str,
    pub guid: GUID,
    pub output: DXGI_FORMAT,
    pub native: NativePictureFormat,
    pub codec: StreamCodecKind,
    pub chroma_format_idc: u8,
    pub bit_depth: u8,
}

pub const H264: DecoderProfile = DecoderProfile {
    name: "h264",
    guid: D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
    output: DXGI_FORMAT_NV12,
    native: NativePictureFormat::Nv12,
    codec: StreamCodecKind::H264,
    chroma_format_idc: 1,
    bit_depth: 8,
};
pub const HEVC_MAIN: DecoderProfile = DecoderProfile {
    name: "hevc-main",
    guid: D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
    output: DXGI_FORMAT_NV12,
    native: NativePictureFormat::Nv12,
    codec: StreamCodecKind::H265,
    chroma_format_idc: 1,
    bit_depth: 8,
};
pub const HEVC_MAIN10: DecoderProfile = DecoderProfile {
    name: "hevc-main10",
    guid: D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
    output: DXGI_FORMAT_P010,
    native: NativePictureFormat::P010,
    codec: StreamCodecKind::H265,
    chroma_format_idc: 1,
    bit_depth: 10,
};
pub const HEVC_MAIN_444: DecoderProfile = DecoderProfile {
    name: "hevc-main-444",
    guid: HEVC_VLD_MAIN_444,
    output: DXGI_FORMAT_AYUV,
    native: NativePictureFormat::Ayuv,
    codec: StreamCodecKind::H265,
    chroma_format_idc: 3,
    bit_depth: 8,
};
pub const HEVC_MAIN10_444: DecoderProfile = DecoderProfile {
    name: "hevc-main10-444",
    guid: HEVC_VLD_MAIN10_444,
    output: DXGI_FORMAT_Y410,
    native: NativePictureFormat::Y410,
    codec: StreamCodecKind::H265,
    chroma_format_idc: 3,
    bit_depth: 10,
};

/// Every profile, in probe order.
pub const PROFILES: [DecoderProfile; 5] =
    [H264, HEVC_MAIN, HEVC_MAIN10, HEVC_MAIN_444, HEVC_MAIN10_444];

/// The profile that decodes `format`, or why none does.
///
/// # Errors
///
/// A user-facing reason for a chroma format or depth DXVA has no profile for.
pub fn profile_for(format: &StreamFormat) -> Result<DecoderProfile, String> {
    let depth = format.bit_depth_luma.max(format.bit_depth_chroma);
    PROFILES
        .into_iter()
        .find(|profile| {
            profile.codec == format.codec
                && profile.chroma_format_idc == format.chroma_format_idc
                && profile.bit_depth == depth
        })
        .ok_or_else(|| {
            format!(
                "no hardware decode profile for {:?} chroma_format_idc {} at {depth} bits",
                format.codec, format.chroma_format_idc
            )
        })
}

/// One profile and what its probe proved (or why it was refused).
pub type ProfileProbe = (DecoderProfile, Result<(), String>);

/// A D3D11 device with video support on one adapter.
#[derive(Debug, Clone)]
pub struct VideoDevice {
    pub device: ID3D11Device,
    pub video: ID3D11VideoDevice,
    /// Held across every multi-call sequence on the immediate context (the
    /// presenters' `present::Gpu::context_lock` when the decoder shares
    /// their device, its own otherwise).
    pub context_lock: std::sync::Arc<std::sync::Mutex<()>>,
    pub adapter_name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// The user-mode driver version (four fields, most significant first),
    /// when known.
    pub driver_version: Option<[u16; 4]>,
}

/// The adapter's user-mode driver version, as Windows reports it.
#[must_use]
pub fn driver_version(adapter: &IDXGIAdapter1) -> Option<[u16; 4]> {
    // SAFETY: a read-only query; the GUID outlives the call.
    #[allow(unsafe_code)]
    let packed =
        unsafe { adapter.CheckInterfaceSupport(&windows::Win32::Graphics::Dxgi::IDXGIDevice::IID) }
            .ok()?;
    let packed = packed.cast_unsigned();
    let field = |shift: u32| u16::try_from((packed >> shift) & 0xFFFF).unwrap_or(0);
    Some([field(48), field(32), field(16), field(0)])
}

/// A hardware adapter: interface, name, PCI vendor and device id, LUID.
pub type HardwareAdapter = (IDXGIAdapter1, String, u32, u32, LUID);

/// The hardware adapters, in DXGI order (software adapters left out).
///
/// # Errors
///
/// DXGI factory failure.
pub fn hardware_adapters() -> windows::core::Result<Vec<HardwareAdapter>> {
    let mut adapters = Vec::new();
    // SAFETY: plain DXGI enumeration; every returned interface is owned.
    #[allow(unsafe_code)]
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            index += 1;
            let desc = adapter.GetDesc1()?;
            if desc.Flags & u32::try_from(DXGI_ADAPTER_FLAG_SOFTWARE.0).unwrap_or(0) != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_owned();
            adapters.push((
                adapter,
                name,
                desc.VendorId,
                desc.DeviceId,
                desc.AdapterLuid,
            ));
        }
    }
    Ok(adapters)
}

impl VideoDevice {
    /// Creates a device with video and BGRA support on `adapter`, with
    /// multithread protection on (lab readbacks may share it with the
    /// decoder's thread).
    ///
    /// # Errors
    ///
    /// Device creation failure, or no `ID3D11VideoDevice`.
    pub fn create(
        adapter: &IDXGIAdapter1,
        adapter_name: String,
        vendor_id: u32,
        device_id: u32,
    ) -> windows::core::Result<Self> {
        let levels: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device = None;
        // SAFETY: standard device creation with owned out-parameters.
        #[allow(unsafe_code)]
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        Self::from_device(device, adapter_name, vendor_id, device_id)
    }

    /// Wraps an existing device (the presenter's), which must have been
    /// created with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`.
    ///
    /// # Errors
    ///
    /// No `ID3D11VideoDevice` on this device.
    pub fn from_device(
        device: ID3D11Device,
        adapter_name: String,
        vendor_id: u32,
        device_id: u32,
    ) -> windows::core::Result<Self> {
        let video: ID3D11VideoDevice = device.cast()?;
        // SAFETY: querying and flipping the documented protection flag.
        #[allow(unsafe_code)]
        unsafe {
            if let Ok(multithread) = device
                .GetImmediateContext()
                .and_then(|context| context.cast::<ID3D11Multithread>())
            {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        Ok(Self {
            device,
            video,
            context_lock: std::sync::Arc::default(),
            adapter_name,
            vendor_id,
            device_id,
            driver_version: None,
        })
    }

    /// The decoder side of the presenter's device: same device, same
    /// context lock.
    ///
    /// # Errors
    ///
    /// The device was created without video support.
    pub fn from_gpu(gpu: &crate::present::Gpu) -> windows::core::Result<Self> {
        let mut device = Self::from_device(gpu.device.clone(), gpu.adapter_name.clone(), 0, 0)?;
        device.context_lock = std::sync::Arc::clone(&gpu.context_lock);
        Ok(device)
    }

    /// A device of the decoder's own on the presenter's adapter (the same
    /// LUID): its own immediate context, so nothing a presenter does on its
    /// device can stall decode (`super::shared`).
    ///
    /// # Errors
    ///
    /// The adapter is gone or has no video device.
    pub fn own_for(gpu: &crate::present::Gpu) -> Result<Self, String> {
        let adapters = hardware_adapters().map_err(|error| format!("adapters: {error}"))?;
        let (adapter, name, vendor, device, _) = adapters
            .into_iter()
            .find(|(_, _, _, _, luid)| *luid == gpu.luid)
            .ok_or("the presenter's adapter is not a hardware adapter")?;
        let mut created = Self::create(&adapter, name, vendor, device)
            .map_err(|error| format!("decode device: {error}"))?;
        created.driver_version = driver_version(&adapter);
        Ok(created)
    }

    /// The decoder configuration Arcen uses for `profile` at the given
    /// size: unencrypted, short slice format (`ConfigBitstreamRaw` 2 for
    /// H.264, 1 or 2 for HEVC).
    ///
    /// # Errors
    ///
    /// A reason when the adapter has no usable configuration.
    pub fn configuration(
        &self,
        profile: &DecoderProfile,
        width: u32,
        height: u32,
    ) -> Result<(D3D11_VIDEO_DECODER_DESC, D3D11_VIDEO_DECODER_CONFIG), String> {
        let desc = D3D11_VIDEO_DECODER_DESC {
            Guid: profile.guid,
            SampleWidth: width,
            SampleHeight: height,
            OutputFormat: profile.output,
        };
        // SAFETY: read-only capability queries with owned out-parameters.
        #[allow(unsafe_code)]
        unsafe {
            if !self.lists_profile(&profile.guid) {
                return Err(format!(
                    "{} is not offered by {}",
                    profile.name, self.adapter_name
                ));
            }
            if !self
                .video
                .CheckVideoDecoderFormat(&profile.guid, profile.output)
                .is_ok_and(windows::Win32::Foundation::BOOL::as_bool)
            {
                return Err(format!(
                    "{} cannot output {:?} on {}",
                    profile.name, profile.output, self.adapter_name
                ));
            }
            let count = self.video.GetVideoDecoderConfigCount(&desc).unwrap_or(0);
            let mut best: Option<(u32, D3D11_VIDEO_DECODER_CONFIG)> = None;
            for index in 0..count {
                let mut config = D3D11_VIDEO_DECODER_CONFIG::default();
                if self
                    .video
                    .GetVideoDecoderConfig(&desc, index, &mut config)
                    .is_err()
                {
                    continue;
                }
                if config.guidConfigBitstreamEncryption != DXVA_NO_ENCRYPT
                    && config.guidConfigBitstreamEncryption != GUID::zeroed()
                {
                    continue;
                }
                let score = match (profile.codec, config.ConfigBitstreamRaw) {
                    (StreamCodecKind::H264, 2) | (StreamCodecKind::H265, 1) => 2,
                    (StreamCodecKind::H265, 2) => 1,
                    _ => 0,
                };
                if score > 0 && best.is_none_or(|(current, _)| score > current) {
                    best = Some((score, config));
                }
            }
            best.map(|(_, config)| (desc, config)).ok_or_else(|| {
                format!(
                    "{} has no short-slice configuration at {width}x{height} on {}",
                    profile.name, self.adapter_name
                )
            })
        }
    }

    /// Whether the adapter lists the decode profile `guid` at all.
    #[must_use]
    pub fn lists_profile(&self, guid: &GUID) -> bool {
        // SAFETY: read-only capability queries with owned out-parameters.
        #[allow(unsafe_code)]
        unsafe {
            (0..self.video.GetVideoDecoderProfileCount())
                .filter_map(|index| self.video.GetVideoDecoderProfile(index).ok())
                .any(|listed| listed == *guid)
        }
    }

    /// Whether the GPU has an AV1 (Profile 0) decoder. Listed only: this
    /// Deck has no AV1 decoder of its own yet, so AV1 is never advertised;
    /// the answer only makes its reason exact.
    #[must_use]
    pub fn lists_av1(&self) -> bool {
        self.lists_profile(&D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0)
    }

    /// Probes one profile: configuration plus a real decoder at the probe
    /// size.
    ///
    /// # Errors
    ///
    /// The refusal reason.
    pub fn probe(&self, profile: &DecoderProfile) -> Result<(), String> {
        let (desc, config) = self.configuration(profile, PROBE_WIDTH, PROBE_HEIGHT)?;
        // SAFETY: creates and drops a decoder object.
        #[allow(unsafe_code)]
        unsafe {
            self.video
                .CreateVideoDecoder(&desc, &config)
                .map(drop)
                .map_err(|error| format!("{} decoder creation failed: {error}", profile.name))
        }
    }

    /// What this adapter proved, for the hello.
    #[must_use]
    pub fn capabilities(&self) -> (DecodeCapabilities, Vec<ProfileProbe>) {
        let results: Vec<ProfileProbe> = PROFILES
            .iter()
            .map(|profile| (*profile, self.probe(profile)))
            .collect();
        let ok = |wanted: &DecoderProfile| {
            results
                .iter()
                .any(|(profile, result)| profile == wanted && result.is_ok())
        };
        let capabilities = DecodeCapabilities {
            backend: DecoderBackendFamily::D3d11Video,
            h264: ok(&H264),
            h265: ok(&HEVC_MAIN),
            av1: false,
            // Grading and HDR are HEVC Main 4:4:4 10.
            yuv444: ok(&HEVC_MAIN10_444),
            main10: ok(&HEVC_MAIN10),
            main12: false,
            full_range: true,
            identity_matrix: false,
            bt601_matrix: true,
            bt2020_ncl_matrix: true,
            hardware_accelerated: Some(true),
        };
        (capabilities, results)
    }
}
