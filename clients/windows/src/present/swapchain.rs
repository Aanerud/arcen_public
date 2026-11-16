// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The video swapchain: a flip-model composition swapchain for the video
//! visual, one pixel format and colour space per contract.
//!
//! | Mode       | Format              | Colour space            | Metadata  |
//! |------------|---------------------|-------------------------|-----------|
//! | Sdr8       | B8G8R8A8_UNORM      | RGB_FULL_G22_NONE_P709   | none      |
//! | Grading10  | R10G10B10A2_UNORM   | RGB_FULL_G22_NONE_P709   | none      |
//! | Hdr10Pq    | R10G10B10A2_UNORM   | RGB_FULL_G2084_NONE_P2020| HDR10     |
//! | ScrgbFp16  | R16G16B16A16_FLOAT  | RGB_FULL_G10_NONE_P709   | HDR10     |
//!
//! Transfer decides HDR, as on the Mac: PQ only when the host returned PQ;
//! HDR metadata is withdrawn before an SDR retag.

use super::convert::OutputKind;
use super::device::Gpu;
use super::{Result, hr};
use arcen_media::BitDepth;
use arcen_media::video::PresentTiming;
use arcen_media::video::present::{
    HDR10_MASTERING_MAX_NITS, HDR10_MASTERING_MIN_NITS, VideoColorContract, presents_pq,
};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::Interface;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentMode {
    Sdr8,
    Grading10,
    Hdr10Pq,
    ScrgbFp16,
}

impl PresentMode {
    /// The mode a decoded stream is presented in. Depth picks the 8-bit or
    /// the 10-bit pipeline; only a PQ transfer makes it HDR, and the FP16
    /// scRGB path replaces HDR10 only when `scrgb_fallback` is set.
    #[must_use]
    pub fn for_contract(contract: VideoColorContract, scrgb_fallback: bool) -> Self {
        match (contract.depth, presents_pq(contract.transfer)) {
            (BitDepth::Eight, false) => Self::Sdr8,
            (_, true) if scrgb_fallback => Self::ScrgbFp16,
            (_, true) => Self::Hdr10Pq,
            (_, false) => Self::Grading10,
        }
    }

    pub fn format(self) -> DXGI_FORMAT {
        match self {
            Self::Sdr8 => DXGI_FORMAT_B8G8R8A8_UNORM,
            Self::Grading10 | Self::Hdr10Pq => DXGI_FORMAT_R10G10B10A2_UNORM,
            Self::ScrgbFp16 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        }
    }
    pub fn color_space(self) -> DXGI_COLOR_SPACE_TYPE {
        match self {
            Self::Sdr8 | Self::Grading10 => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            Self::Hdr10Pq => DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
            Self::ScrgbFp16 => DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
        }
    }
    pub fn is_hdr(self) -> bool {
        matches!(self, Self::Hdr10Pq | Self::ScrgbFp16)
    }
    pub fn output(self) -> OutputKind {
        match self {
            Self::ScrgbFp16 => OutputKind::PqToScrgb,
            _ => OutputKind::Codes,
        }
    }
    pub fn bytes_per_texel(self) -> u32 {
        if self == Self::ScrgbFp16 { 8 } else { 4 }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Sdr8 => "sdr8",
            Self::Grading10 => "grading10",
            Self::Hdr10Pq => "hdr10-pq",
            Self::ScrgbFp16 => "scrgb-fp16",
        }
    }
}

/// HDR10 static metadata with the Mac's reference grade: BT.2020
/// primaries, D65, mastering 0.005–1000 nits, content light unknown (0).
pub fn hdr10_metadata() -> DXGI_HDR_METADATA_HDR10 {
    let xy = |x: f64, y: f64| [(x * 50_000.0).round() as u16, (y * 50_000.0).round() as u16];
    DXGI_HDR_METADATA_HDR10 {
        RedPrimary: xy(0.708, 0.292),
        GreenPrimary: xy(0.170, 0.797),
        BluePrimary: xy(0.131, 0.046),
        WhitePoint: xy(0.3127, 0.3290),
        MaxMasteringLuminance: HDR10_MASTERING_MAX_NITS as u32,
        // 0.0001-nit units.
        MinMasteringLuminance: (HDR10_MASTERING_MIN_NITS * 10_000.0).round() as u32,
        MaxContentLightLevel: 0,
        MaxFrameAverageLightLevel: 0,
    }
}

pub struct VideoSwapchain {
    pub swapchain: IDXGISwapChain4,
    pub waitable: HANDLE,
    pub mode: PresentMode,
    pub width: u32,
    pub height: u32,
    pub tearing_supported: bool,
    flags: u32,
    applied: Option<PresentMode>,
}

pub fn tearing_supported(gpu: &Gpu) -> bool {
    let Ok(factory5) = gpu.factory.cast::<IDXGIFactory5>() else {
        return false;
    };
    let mut allow = windows::Win32::Foundation::BOOL(0);
    unsafe {
        factory5
            .CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                std::ptr::from_mut(&mut allow).cast(),
                std::mem::size_of::<windows::Win32::Foundation::BOOL>() as u32,
            )
            .is_ok()
            && allow.as_bool()
    }
}

impl VideoSwapchain {
    pub fn create(gpu: &Gpu, width: u32, height: u32, mode: PresentMode) -> Result<Self> {
        let tearing = tearing_supported(gpu);
        // ALLOW_TEARING cannot be added later: always created when possible,
        // passed per present only when the timing allows tearing (Speed).
        let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
        if tearing {
            flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
        }
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: mode.format(),
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags,
        };
        let swapchain = hr("CreateSwapChainForComposition", unsafe {
            gpu.factory
                .CreateSwapChainForComposition(&gpu.device, &desc, None)
        })?;
        let swapchain: IDXGISwapChain4 = hr("IDXGISwapChain4", swapchain.cast())?;
        hr("SetMaximumFrameLatency", unsafe {
            swapchain.SetMaximumFrameLatency(1)
        })?;
        let waitable = unsafe { swapchain.GetFrameLatencyWaitableObject() };
        let mut this = Self {
            swapchain,
            waitable,
            mode,
            width,
            height,
            tearing_supported: tearing,
            flags,
            applied: None,
        };
        this.apply_color(mode)?;
        Ok(this)
    }

    /// Which colour spaces DXGI says this swapchain can present.
    pub fn support(&self, space: DXGI_COLOR_SPACE_TYPE) -> u32 {
        unsafe { self.swapchain.CheckColorSpaceSupport(space) }.unwrap_or(0)
    }

    fn apply_color(&mut self, mode: PresentMode) -> Result<()> {
        if self.applied == Some(mode) {
            return Ok(());
        }
        let space = mode.color_space();
        let support = self.support(space);
        if support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 == 0 {
            return Err(format!(
                "{} cannot present {} (support flags {support:#x})",
                mode.name(),
                super::display::color_space_name(space)
            ));
        }
        if mode.is_hdr() {
            hr("SetColorSpace1", unsafe {
                self.swapchain.SetColorSpace1(space)
            })?;
            let metadata = hdr10_metadata();
            hr("SetHDRMetaData(HDR10)", unsafe {
                self.swapchain.SetHDRMetaData(
                    DXGI_HDR_METADATA_TYPE_HDR10,
                    Some(std::slice::from_raw_parts(
                        std::ptr::from_ref(&metadata).cast::<u8>(),
                        std::mem::size_of::<DXGI_HDR_METADATA_HDR10>(),
                    )),
                )
            })?;
        } else {
            // Withdraw HDR before retagging SDR.
            let _ = unsafe {
                self.swapchain
                    .SetHDRMetaData(DXGI_HDR_METADATA_TYPE_NONE, None)
            };
            hr("SetColorSpace1", unsafe {
                self.swapchain.SetColorSpace1(space)
            })?;
        }
        self.applied = Some(mode);
        Ok(())
    }

    /// Switches contract: new format (ResizeBuffers) and colour space.
    pub fn set_mode(&mut self, mode: PresentMode, width: u32, height: u32) -> Result<()> {
        if mode.format() != self.mode.format() || width != self.width || height != self.height {
            if self.mode.is_hdr() && !mode.is_hdr() {
                let _ = unsafe {
                    self.swapchain
                        .SetHDRMetaData(DXGI_HDR_METADATA_TYPE_NONE, None)
                };
            }
            hr("ResizeBuffers", unsafe {
                self.swapchain.ResizeBuffers(
                    0,
                    width,
                    height,
                    mode.format(),
                    DXGI_SWAP_CHAIN_FLAG(self.flags as i32),
                )
            })?;
            self.width = width;
            self.height = height;
            self.applied = None;
        }
        self.mode = mode;
        self.apply_color(mode)
    }

    pub fn back_buffer(&self) -> Result<ID3D11Texture2D> {
        hr("GetBuffer", unsafe {
            self.swapchain.GetBuffer::<ID3D11Texture2D>(0)
        })
    }

    /// Waits for the swapchain to accept a frame (frame latency 1).
    pub fn wait(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObjectEx(self.waitable, timeout_ms, true) == WAIT_OBJECT_0 }
    }

    /// Presents the back buffer with the served pipeline's `timing`
    /// ([`arcen_media::video::PresentTiming`]):
    ///
    /// - `Vsync`: sync interval 1, every frame shown for a refresh.
    /// - `Newest`: sync interval 0 without `ALLOW_TEARING`. For the flip
    ///   model that cancels the rest of the previous frame's refresh and
    ///   discards this frame if a newer one is queued: the newest frame wins
    ///   at the next vblank or composition, never torn (`IDXGISwapChain::Present`,
    ///   `SyncInterval`).
    /// - `Tearing`: sync interval 0 with `DXGI_PRESENT_ALLOW_TEARING` where
    ///   DXGI supports it. A frame tears only where DWM flips this swapchain
    ///   directly; composed, it is shown at DWM's next composition.
    pub fn present(&self, timing: PresentTiming) -> Result<()> {
        let flags = if timing.allows_tearing() && self.tearing_supported {
            DXGI_PRESENT_ALLOW_TEARING
        } else {
            DXGI_PRESENT(0)
        };
        let interval = u32::from(timing.waits_for_vsync());
        hr(
            "Present",
            unsafe { self.swapchain.Present(interval, flags) }.ok(),
        )
    }

    /// The number of the last present (`GetLastPresentCount`).
    pub fn last_present_id(&self) -> Option<u32> {
        unsafe { self.swapchain.GetLastPresentCount() }.ok()
    }

    /// Whether present number `id` was displayed, from DXGI frame
    /// statistics (`PresentCount` is the last present shown).
    pub fn confirm(&self, id: u32) -> super::compose::Confirmation {
        use super::compose::Confirmation;
        let mut stats = DXGI_FRAME_STATISTICS::default();
        match unsafe { self.swapchain.GetFrameStatistics(&mut stats) } {
            Ok(()) if stats.PresentCount.wrapping_sub(id) < u32::MAX / 2 => Confirmation::Displayed,
            Ok(()) => Confirmation::Pending,
            Err(_) => Confirmation::Unconfirmed,
        }
    }

    /// DWM's composition path for the last presents, with the raw reading.
    pub fn composition(&self) -> (super::depth::CompositionPath, String) {
        use super::depth::CompositionPath;
        let Ok(media) = self.swapchain.cast::<IDXGISwapChainMedia>() else {
            return (
                CompositionPath::Unknown,
                "IDXGISwapChainMedia unavailable".into(),
            );
        };
        let mut stats = DXGI_FRAME_STATISTICS_MEDIA::default();
        match unsafe { media.GetFrameStatisticsMedia(&mut stats) } {
            Ok(()) => {
                let (path, name) = match stats.CompositionMode {
                    DXGI_FRAME_PRESENTATION_MODE_COMPOSED => {
                        (CompositionPath::Composed, "composed")
                    }
                    DXGI_FRAME_PRESENTATION_MODE_OVERLAY => (CompositionPath::Direct, "overlay"),
                    DXGI_FRAME_PRESENTATION_MODE_NONE => {
                        (CompositionPath::Direct, "none (independent flip)")
                    }
                    DXGI_FRAME_PRESENTATION_MODE_COMPOSITION_FAILURE => {
                        (CompositionPath::Composed, "composition_failure")
                    }
                    _ => (CompositionPath::Unknown, "unknown"),
                };
                (
                    path,
                    format!(
                        "{name} (approved present duration {})",
                        stats.ApprovedPresentDuration
                    ),
                )
            }
            Err(error) => (
                CompositionPath::Unknown,
                format!("GetFrameStatisticsMedia: {error}"),
            ),
        }
    }
}

super::opaque_debug!(VideoSwapchain);

/// Headless proof: every mode's swapchain is created, tagged, drawn and
/// read back; returns one line per mode.
pub fn probe(gpu: &Gpu) -> Vec<String> {
    let mut lines = Vec::new();
    for mode in [
        PresentMode::Sdr8,
        PresentMode::Grading10,
        PresentMode::Hdr10Pq,
        PresentMode::ScrgbFp16,
    ] {
        match VideoSwapchain::create(gpu, 256, 64, mode) {
            Ok(chain) => {
                let spaces = [
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
                ];
                let support: Vec<String> = spaces
                    .iter()
                    .map(|&space| {
                        format!(
                            "{}={:#x}",
                            super::display::color_space_name(space),
                            chain.support(space)
                        )
                    })
                    .collect();
                lines.push(format!(
                    "swapchain {} format={:?} applied={} tearing={} support[{}]",
                    mode.name(),
                    mode.format(),
                    super::display::color_space_name(mode.color_space()),
                    chain.tearing_supported,
                    support.join(", ")
                ));
            }
            Err(error) => lines.push(format!("swapchain {} FAILED: {error}", mode.name())),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{
        ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
    };

    #[test]
    fn hdr10_metadata_carries_the_shared_grade_in_dxgi_units() {
        let metadata = hdr10_metadata();
        assert_eq!(metadata.MaxMasteringLuminance, 1000);
        assert_eq!(metadata.MinMasteringLuminance, 50);
    }

    #[test]
    fn transfer_not_depth_decides_hdr() {
        let auto = VideoColorContract::default();
        assert_eq!(PresentMode::for_contract(auto, false), PresentMode::Sdr8);
        let grading = VideoColorContract {
            chroma: ChromaSubsampling::Yuv444,
            range: ColorRange::Full,
            depth: BitDepth::Ten,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
        };
        assert_eq!(
            PresentMode::for_contract(grading, false),
            PresentMode::Grading10
        );
        assert_eq!(
            PresentMode::for_contract(grading, true),
            PresentMode::Grading10
        );
        // BT.2020 primaries alone are wide-gamut SDR, not HDR.
        let wide = VideoColorContract {
            primaries: ColorPrimaries::Bt2020,
            matrix: ColorMatrix::Bt2020Ncl,
            ..grading
        };
        assert_eq!(
            PresentMode::for_contract(wide, false),
            PresentMode::Grading10
        );
        let hdr = VideoColorContract {
            transfer: TransferCharacteristics::Pq,
            ..wide
        };
        assert_eq!(PresentMode::for_contract(hdr, false), PresentMode::Hdr10Pq);
        assert_eq!(PresentMode::for_contract(hdr, true), PresentMode::ScrgbFp16);
        assert!(!PresentMode::Grading10.is_hdr() && PresentMode::Hdr10Pq.is_hdr());
    }
}
