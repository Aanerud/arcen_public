//! `capenc color-probe`: does this host actually deliver more than 8 bits per
//! component from Desktop Duplication?
//!
//! Read `docs/internal/ten-bit-source-capture.md` before changing this. The
//! short version: Arcen already encodes 10-bit, but from an 8-bit capture, and
//! the open question is whether a wider *source* is reachable at all. Public
//! NvFBC on Linux is a closed negative. Windows is the one live lead, because
//! `IDXGIOutput5::DuplicateOutput1` accepts a caller-supplied format list where
//! `IDXGIOutput1::DuplicateOutput` always flattens the desktop to 8-bit BGRA.
//!
//! This probe deliberately proves nothing on its own about *support*. It
//! answers one question with evidence: for each output, which format does the
//! duplication actually return, and do the captured samples carry information
//! below the eighth bit.
//!
//! # Why the returned format is not the requested format
//!
//! Microsoft documents `DuplicateOutput1` as choosing one of the caller's
//! formats, but there are field reports of `R16G16B16A16_FLOAT` arriving when
//! only `B8G8R8A8_UNORM` was listed. So every decision here branches on
//! `GetDesc().ModeDesc.Format`, never on what was asked for.
//!
//! # Why a format alone is not the answer
//!
//! A 10-bit container can carry 8-bit data. Windows can widen an 8-bit desktop
//! into `R10G10B10A2` by shifting left two bits, or by replicating the high
//! bits into the low ones. Both produce a buffer that passes every size and
//! format check while carrying no extra information. The classifier below
//! looks for exactly those two shapes and reports `container-only` when it
//! finds them, because that is the failure this whole question exists to avoid.

use windows::core::Interface;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput, IDXGIOutput5, IDXGIOutput6,
    IDXGIOutputDuplication, IDXGIResource, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::log;

/// Formats to offer the duplication, widest first.
///
/// FP16 leads because Windows Advanced Color composes in FP16 scRGB, so it is
/// the only format that can carry an HDR desktop without narrowing it. The
/// 8-bit entry is mandatory: without a format the system can always satisfy,
/// `DuplicateOutput1` has no legal fallback and fails outright.
const REQUESTED_FORMATS: [DXGI_FORMAT; 3] = [
    DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_B8G8R8A8_UNORM,
];

/// Make the process per-monitor DPI aware before duplicating.
///
/// `DuplicateOutput1` returns `DXGI_ERROR_UNSUPPORTED` for a process that is
/// not per-monitor DPI aware, which is indistinguishable from "this host
/// cannot give you a wider format". Without this call the probe cannot tell a
/// real negative from its own defect, and a wrong negative here would close a
/// line of work that is still open.
///
/// Resolved dynamically rather than by adding a `windows` crate feature, the
/// same way this crate already resolves `nvEncodeAPI64.dll`.
unsafe fn enable_per_monitor_dpi_awareness() -> bool {
    let module = match LoadLibraryA(windows::core::s!("user32.dll")) {
        Ok(module) => module,
        Err(_) => return false,
    };
    let Some(symbol) = GetProcAddress(module, windows::core::s!("SetProcessDpiAwarenessContext"))
    else {
        return false;
    };
    // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 is the sentinel value -4.
    type SetContext = unsafe extern "system" fn(isize) -> i32;
    let set: SetContext = std::mem::transmute(symbol);
    set(-4) != 0
}

/// Try one format list and report exactly what came back.
///
/// Returns `Some(returned_format)` when duplication succeeded.
unsafe fn try_duplicate(
    output5: &IDXGIOutput5,
    device: &ID3D11Device,
    label: &str,
    formats: &[DXGI_FORMAT],
) -> Option<(IDXGIOutputDuplication, DXGI_FORMAT)> {
    match output5.DuplicateOutput1(device, 0, formats) {
        Ok(duplication) => {
            let returned = duplication.GetDesc().ModeDesc.Format;
            log(&format!(
                "    [{label}] ok -> returned {}",
                format_name(returned)
            ));
            Some((duplication, returned))
        }
        Err(error) => {
            log(&format!(
                "    [{label}] failed: {:?} {}",
                error.code(),
                error.message()
            ));
            None
        }
    }
}

fn format_name(format: DXGI_FORMAT) -> &'static str {
    match format {
        DXGI_FORMAT_R16G16B16A16_FLOAT => "R16G16B16A16_FLOAT",
        DXGI_FORMAT_R10G10B10A2_UNORM => "R10G10B10A2_UNORM",
        DXGI_FORMAT_B8G8R8A8_UNORM => "B8G8R8A8_UNORM",
        _ => "other",
    }
}

/// What the sampled pixels say about the source, independent of the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Depth {
    /// Every low bit is zero: an 8-bit source shifted left into a wider word.
    ContainerOnlyZeroFilled,
    /// Low bits mirror the high bits: an 8-bit source widened by replication.
    ContainerOnlyBitReplicated,
    /// Low bits carry information neither zero nor replicated.
    GenuinelyWide,
    /// The container is 8-bit, so there is nothing below the eighth bit.
    EightBitContainer,
    /// A wide container whose samples all land on the 8-bit grid.
    EightBitContentInWideContainer,
    /// Not enough distinct samples to judge — a static black desktop, usually.
    Inconclusive,
}

impl Depth {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ContainerOnlyZeroFilled => "CONTAINER ONLY (8-bit shifted, low bits all zero)",
            Self::ContainerOnlyBitReplicated => "CONTAINER ONLY (8-bit bit-replicated)",
            Self::GenuinelyWide => "GENUINE >8bpc INFORMATION",
            Self::EightBitContainer => "8-bit container",
            Self::EightBitContentInWideContainer => {
                "WIDE CONTAINER, 8-bit content (samples land on the 8-bit sRGB grid)"
            }
            Self::Inconclusive => "INCONCLUSIVE (too few distinct samples; show real content)",
        }
    }
}

/// Classify `R10G10B10A2_UNORM` samples.
///
/// Split out and pure so the decision can be unit-tested on any host; the FFI
/// above it cannot be.
pub(crate) fn classify_r10g10b10a2(pixels: &[u32]) -> Depth {
    let mut distinct = std::collections::BTreeSet::new();
    let mut any_low_bits = false;
    let mut all_replicated = true;
    for &px in pixels {
        for shift in [0u32, 10, 20] {
            let channel = (px >> shift) & 0x3FF;
            distinct.insert(channel);
            let low = channel & 0x3;
            if low != 0 {
                any_low_bits = true;
            }
            // An 8-bit value widened by replication has its top two bits
            // repeated in the bottom two.
            if low != ((channel >> 8) & 0x3) {
                all_replicated = false;
            }
        }
    }
    if distinct.len() < 8 {
        return Depth::Inconclusive;
    }
    if !any_low_bits {
        return Depth::ContainerOnlyZeroFilled;
    }
    if all_replicated {
        return Depth::ContainerOnlyBitReplicated;
    }
    Depth::GenuinelyWide
}

/// Convert an IEEE-754 binary16 to `f32`.
fn half_to_f32(bits: u16) -> f32 {
    let sign = ((bits >> 15) & 1) as u32;
    let exponent = ((bits >> 10) & 0x1F) as u32;
    let mantissa = (bits & 0x3FF) as u32;
    let out = match exponent {
        0 if mantissa == 0 => sign << 31,
        // Subnormal: renormalise into a binary32 exponent.
        0 => {
            let mut e = exponent;
            let mut m = mantissa;
            while m & 0x400 == 0 {
                m <<= 1;
                e = e.wrapping_sub(1);
            }
            let e = (e.wrapping_add(1)).wrapping_add(127 - 15);
            (sign << 31) | (e << 23) | ((m & 0x3FF) << 13)
        }
        0x1F => (sign << 31) | (0xFF << 23) | (mantissa << 13),
        _ => (sign << 31) | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(out)
}

/// The sRGB encoding transfer function, linear light in, signal out.
fn linear_to_srgb(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// Classify `R16G16B16A16_FLOAT` (scRGB) samples.
///
/// Counting distinct half-floats — the obvious approach, and this function's
/// first implementation — is worthless here, and worse than worthless because
/// it reports success. The desktop compositor works in float, so antialiasing
/// and blending of ordinary 8-bit content produce far more than 256 distinct
/// values without a single one of them carrying information the 8-bit source
/// did not already have. That first version reported "genuine >8bpc" on an SDR
/// desktop, which is exactly the false positive this whole investigation is
/// supposed to avoid.
///
/// The question that actually distinguishes the two: does each sample land on
/// the 8-bit sRGB grid? Convert scRGB linear light back through the sRGB
/// transfer function to a 0..255 signal. Content that originated as 8-bit
/// lands on integers within float tolerance. Content carrying more than 8 bits
/// lands between them, systematically rather than occasionally.
/// Estimate quantisation levels from the sample spacing. **Reported, never
/// decisive** — read the caveat before using it for anything.
///
/// Why it is here: the sRGB-grid test in [`classify_fp16`] is only valid while
/// Advanced Color is **off**. In HDR composition Windows maps SDR content into
/// scRGB scaled by the SDR white level, so ordinary 8-bit content lands off the
/// 8-bit grid while carrying no more information. Under HDR the grid verdict
/// must not be believed.
///
/// Why it is not the answer either: on uniformly spaced samples the range/step
/// ratio is scale-invariant, which is the property wanted. But scRGB carries
/// *linear light*, and 8-bit sRGB codes converted to linear are emphatically
/// not uniformly spaced — the gaps near black are orders of magnitude smaller
/// than those near white, so this reads ordinary 8-bit content as
/// high-precision. A unit test pins that failure so nobody promotes this to a
/// verdict without solving it.
///
/// So it is logged as evidence beside the grid verdict, and a wide-source claim
/// under HDR needs a human reading both. Solving it properly means recovering
/// the SDR white scale and testing uniformity in the signal domain, which needs
/// a known reference in frame.
///
/// Returns `None` when the samples are too uniform to estimate.
pub(crate) fn estimate_levels(values: &[f32]) -> Option<f32> {
    let mut sorted: Vec<f32> = values
        .iter()
        .copied()
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    if sorted.len() < 64 {
        return None;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let range = sorted[sorted.len() - 1] - sorted[0];
    if range <= 0.0 {
        return None;
    }
    // Smallest meaningful gap between neighbouring distinct values. Taking a
    // low percentile rather than the true minimum keeps one unlucky pair of
    // near-identical floats from claiming an implausibly fine quantisation.
    let mut gaps: Vec<f32> = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g > range * 1e-6)
        .collect();
    if gaps.len() < 16 {
        return None;
    }
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let step = gaps[gaps.len() / 20];
    if step <= 0.0 {
        return None;
    }
    Some(range / step)
}

pub(crate) fn classify_fp16(halfs: &[u16]) -> Depth {
    // Measured, not guessed. Half-float carries ~11 mantissa bits, so even
    // exact 8-bit content acquires some error on the way through scRGB. Taking
    // all 256 sRGB codes through binary16 and back, the worst deviation is
    // 0.037 code units. Doing the same with 1024 ten-bit codes puts 70% of
    // samples beyond 0.15, median 0.25. A 0.10 threshold sits in the gap: no
    // 8-bit sample can reach it, and most 10-bit samples clear it easily.
    const OFF_GRID_TOLERANCE: f32 = 0.10;
    // Blending genuinely does put a minority of composited pixels between grid
    // points, so a handful of off-grid samples is not evidence. Require a
    // substantial share before calling a source wide.
    const OFF_GRID_SHARE_REQUIRED: f32 = 0.25;

    let mut considered = 0usize;
    let mut off_grid = 0usize;
    let mut distinct = std::collections::BTreeSet::new();
    for &bits in halfs {
        let linear = half_to_f32(bits);
        if !linear.is_finite() || !(0.0..=1.0).contains(&linear) {
            // Out-of-range scRGB is real HDR signal, not an 8-bit desktop.
            if linear.is_finite() && linear > 1.0 {
                off_grid += 1;
                considered += 1;
            }
            continue;
        }
        distinct.insert(bits);
        let code = linear_to_srgb(linear) * 255.0;
        if (code - code.round()).abs() > OFF_GRID_TOLERANCE {
            off_grid += 1;
        }
        considered += 1;
    }
    if considered < 64 || distinct.len() < 8 {
        return Depth::Inconclusive;
    }
    let share = off_grid as f32 / considered as f32;
    if share >= OFF_GRID_SHARE_REQUIRED {
        Depth::GenuinelyWide
    } else {
        Depth::EightBitContentInWideContainer
    }
}

unsafe fn sample_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
) -> Result<Depth, String> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    texture.GetDesc(&mut desc);
    let staging = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut copy: Option<ID3D11Texture2D> = None;
    device
        .CreateTexture2D(&staging, None, Some(&mut copy))
        .map_err(|e| format!("create staging texture: {e:?}"))?;
    let copy = copy.ok_or_else(|| "staging texture was not created".to_string())?;
    let src: ID3D11Resource = texture.cast().map_err(|e| format!("cast source: {e:?}"))?;
    let dst: ID3D11Resource = copy.cast().map_err(|e| format!("cast staging: {e:?}"))?;
    context.CopyResource(&dst, &src);

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    context
        .Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .map_err(|e| format!("map staging: {e:?}"))?;

    // Stride across the WHOLE surface rather than reading a corner. A 256x256
    // block from the top-left of a 3008x1692 desktop is mostly empty
    // background, which made the classifier report "inconclusive" on outputs
    // that were in fact carrying content. Sampling the full extent costs the
    // same number of reads and actually represents the frame.
    const SAMPLES_PER_AXIS: u32 = 256;
    let rows = desc.Height.min(SAMPLES_PER_AXIS);
    let cols = desc.Width.min(SAMPLES_PER_AXIS);
    let row_step = (desc.Height / rows.max(1)).max(1);
    let col_step = (desc.Width / cols.max(1)).max(1);
    let verdict = if format == DXGI_FORMAT_R10G10B10A2_UNORM {
        let mut pixels = Vec::with_capacity((rows * cols) as usize);
        for y in 0..rows {
            let row = (mapped.pData as *const u8)
                .add(((y * row_step).min(desc.Height - 1) * mapped.RowPitch) as usize)
                as *const u32;
            for x in 0..cols {
                pixels.push(*row.add((x * col_step).min(desc.Width - 1) as usize));
            }
        }
        classify_r10g10b10a2(&pixels)
    } else if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        let mut halfs = Vec::with_capacity((rows * cols) as usize);
        for y in 0..rows {
            let row = (mapped.pData as *const u8)
                .add(((y * row_step).min(desc.Height - 1) * mapped.RowPitch) as usize)
                as *const u16;
            for x in 0..cols {
                // Red channel only; four halfs per pixel.
                halfs.push(*row.add(((x * col_step).min(desc.Width - 1) * 4) as usize));
            }
        }
        let linear: Vec<f32> = halfs.iter().map(|b| half_to_f32(*b)).collect();
        match estimate_levels(&linear) {
            Some(levels) => log(&format!(
                "      evidence: ~{levels:.0} quantisation levels in red \
                 (diagnostic only; linear light defeats this for 8-bit sRGB)"
            )),
            None => log("      evidence: too uniform to estimate quantisation levels"),
        }
        classify_fp16(&halfs)
    } else {
        Depth::EightBitContainer
    };
    context.Unmap(&dst, 0);
    Ok(verdict)
}

/// Can Windows Graphics Capture supply a wider frame than 8-bit BGRA?
///
/// This matters more than `DuplicateOutput1` on the deployment target. Desktop
/// Duplication opens on a headless vGPU but delivers no desktop images, so
/// `win.rs` falls back to WGC — which means WGC, not DDA, is the capture path
/// actually in use there, and `wgc.rs` hardcodes
/// `B8G8R8A8UIntNormalized`.
///
/// Creating the pool is the whole test: `Direct3D11CaptureFramePool` rejects a
/// pixel format it cannot supply, so a successful creation with a wide format
/// is the first real evidence that a wide source is reachable at all.

/// Start a capture session on `pool`, pull one real frame, and classify its
/// pixels.
///
/// This is the whole point of the WGC probe. `CreateFreeThreaded` succeeding
/// says the runtime accepts the pixel format; it says nothing about whether the
/// desktop behind it carries more than eight bits. Only the samples answer
/// that, which is why this goes to the trouble of running a live session.
unsafe fn capture_and_classify(
    pool: &Direct3D11CaptureFramePool,
    item: &GraphicsCaptureItem,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    format: DirectXPixelFormat,
    deadline_secs: u64,
) -> Result<Depth, String> {
    let session = pool
        .CreateCaptureSession(item)
        .map_err(|e| format!("CreateCaptureSession: {e:?}"))?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetIsBorderRequired(false);
    session
        .StartCapture()
        .map_err(|e| format!("StartCapture: {e:?}"))?;

    // WGC delivers on desktop change. A quiet desktop can take a moment, and
    // an empty result here must not be read as "no wide data".
    let mut verdict = Depth::Inconclusive;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(deadline_secs);
    while std::time::Instant::now() < deadline {
        if let Ok(frame) = pool.TryGetNextFrame() {
            let outcome = (|| -> Result<Depth, String> {
                let surface = frame.Surface().map_err(|e| format!("Surface: {e:?}"))?;
                let access: IDirect3DDxgiInterfaceAccess =
                    surface.cast().map_err(|e| format!("cast access: {e:?}"))?;
                let texture: ID3D11Texture2D = access
                    .GetInterface()
                    .map_err(|e| format!("GetInterface: {e:?}"))?;
                let dxgi_format = if format == DirectXPixelFormat::R16G16B16A16Float {
                    DXGI_FORMAT_R16G16B16A16_FLOAT
                } else {
                    DXGI_FORMAT_B8G8R8A8_UNORM
                };
                sample_frame(device, context, &texture, dxgi_format)
            })();
            let _ = frame.Close();
            match outcome {
                Ok(found) => {
                    verdict = found;
                    if verdict != Depth::Inconclusive {
                        break;
                    }
                }
                Err(error) => {
                    let _ = session.Close();
                    return Err(error);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    let _ = session.Close();
    Ok(verdict)
}

unsafe fn probe_wgc_formats(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    monitor: HMONITOR,
) {
    // Bounded so the whole enable-capture-classify sequence fits inside the
    // 30s EDID hold; a longer wait simply outlives the display it is measuring.
    let deadline_secs = 3;
    let _ = RoInitialize(RO_INIT_MULTITHREADED);
    log("  WGC (the fallback path this host class actually uses):");
    let dxgi_device: IDXGIDevice = match device.cast() {
        Ok(dev) => dev,
        Err(error) => {
            log(&format!("    cast to IDXGIDevice failed: {error:?}"));
            return;
        }
    };
    let d3d_device: IDirect3DDevice = match CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
        .and_then(|inspectable| inspectable.cast())
    {
        Ok(device) => device,
        Err(error) => {
            log(&format!("    WinRT device wrap failed: {error:?}"));
            return;
        }
    };
    let item: GraphicsCaptureItem =
        match windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .and_then(|interop| interop.CreateForMonitor(monitor))
        {
            Ok(item) => item,
            Err(error) => {
                log(&format!("    CreateForMonitor failed: {error:?}"));
                return;
            }
        };
    let size: SizeInt32 = item.Size().unwrap_or(SizeInt32 {
        Width: 1920,
        Height: 1080,
    });
    for (label, format) in [
        ("R16G16B16A16Float", DirectXPixelFormat::R16G16B16A16Float),
        (
            "R10G10B10A2UIntNormalized",
            DirectXPixelFormat::R10G10B10A2UIntNormalized,
        ),
        (
            "B8G8R8A8UIntNormalized (control)",
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
        ),
    ] {
        match Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d_device, format, 2, size) {
            Ok(pool) => {
                log(&format!("    [{label}] frame pool CREATED"));
                // Creating the pool only proves the API accepts the format.
                // The question is what the pixels contain, so actually capture
                // one and look at it.
                match capture_and_classify(&pool, &item, device, context, format, deadline_secs) {
                    Ok(verdict) => log(&format!("      captured frame -> {}", verdict.label())),
                    Err(error) => log(&format!("      capture failed: {error}")),
                }
                let _ = pool.Close();
            }
            Err(error) => log(&format!("    [{label}] rejected: {:?}", error.code())),
        }
    }
}

unsafe fn probe_output(adapter: &IDXGIAdapter, output: &IDXGIOutput, index: u32, wgc_only: bool) {
    let device_name = output
        .GetDesc()
        .map(|d| {
            String::from_utf16_lossy(&d.DeviceName)
                .trim_end_matches('\0')
                .to_string()
        })
        .unwrap_or_else(|_| "<unknown>".to_string());
    log(&format!("output {index}: {device_name}"));

    // Colour metadata first: it is re-queryable state, not a static fact, and
    // it explains a narrow duplication format when one comes back.
    match output.cast::<IDXGIOutput6>() {
        Ok(output6) => match output6.GetDesc1() {
            Ok(desc1) => log(&format!(
                "  IDXGIOutput6: BitsPerColor={} ColorSpace={:?}",
                desc1.BitsPerColor, desc1.ColorSpace
            )),
            Err(error) => log(&format!("  IDXGIOutput6::GetDesc1 failed: {error:?}")),
        },
        Err(_) => log("  IDXGIOutput6: not available on this output"),
    }

    let output5: IDXGIOutput5 = match output.cast() {
        Ok(output5) => output5,
        Err(error) => {
            log(&format!(
                "  IDXGIOutput5: not available ({error:?}) — DuplicateOutput1 is unreachable here"
            ));
            return;
        }
    };

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    if let Err(error) = D3D11CreateDevice(
        adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        None,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    ) {
        log(&format!("  D3D11CreateDevice failed: {error:?}"));
        return;
    }
    let (Some(device), Some(context)) = (device, context) else {
        log("  D3D11CreateDevice returned no device");
        return;
    };

    // Ladder the requests so a failure is attributable. If the 8-bit-only list
    // also fails, the problem is not colour depth and no conclusion about
    // 10-bit support may be drawn from it.
    if wgc_only {
        // The Desktop Duplication ladder is already answered on this hardware
        // and costs seconds per output. Skipping it is what lets the whole
        // sequence finish inside a 30s EDID hold.
        if let Ok(output_desc) = output.GetDesc() {
            probe_wgc_formats(&device, &context, output_desc.Monitor);
        }
        return;
    }
    log("  DuplicateOutput1 attempts:");
    let attempts: [(&str, &[DXGI_FORMAT]); 4] = [
        ("widest: FP16,R10,BGRA8", &REQUESTED_FORMATS),
        ("FP16 only", &[DXGI_FORMAT_R16G16B16A16_FLOAT]),
        ("R10G10B10A2 only", &[DXGI_FORMAT_R10G10B10A2_UNORM]),
        ("BGRA8 only (control)", &[DXGI_FORMAT_B8G8R8A8_UNORM]),
    ];
    // Each attempt must release its duplication before the next one runs. An
    // output can carry only one duplication at a time, so holding the first
    // success made every later attempt fail with E_INVALIDARG and produced a
    // ladder that looked like "only the widest list works" when it actually
    // measured nothing.
    let mut chosen: Option<(String, DXGI_FORMAT)> = None;
    for (label, formats) in attempts {
        match try_duplicate(&output5, &device, label, formats) {
            Some((duplication, format)) => {
                // Keep the narrowest successful one for sampling, but only
                // after every attempt has had an unobstructed try.
                drop(duplication);
                if chosen.is_none() {
                    chosen = Some((label.to_string(), format));
                }
            }
            None => continue,
        }
    }
    let Some((winning_label, returned)) = chosen else {
        log("  VERDICT: DuplicateOutput1 unusable on this output (see attempts above)");
        return;
    };
    // Re-open the duplication we intend to sample, now that nothing holds it.
    let formats: Vec<DXGI_FORMAT> = vec![returned];
    let Some((duplication, returned)) = try_duplicate(
        &output5,
        &device,
        &format!("reopen for sampling ({winning_label})"),
        &formats,
    ) else {
        log("  could not reopen duplication for sampling");
        return;
    };
    let desc = duplication.GetDesc();
    log(&format!(
        "  first successful format: {} ({}x{})",
        format_name(returned),
        desc.ModeDesc.Width,
        desc.ModeDesc.Height
    ));

    if let Ok(output_desc) = output.GetDesc() {
        probe_wgc_formats(&device, &context, output_desc.Monitor);
    }

    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;
    let mut verdict = Depth::Inconclusive;
    for attempt in 0..30 {
        match duplication.AcquireNextFrame(500, &mut info, &mut resource) {
            Ok(()) => {
                if let Some(res) = resource.take() {
                    if let Ok(texture) = res.cast::<ID3D11Texture2D>() {
                        match sample_frame(&device, &context, &texture, returned) {
                            Ok(found) => verdict = found,
                            Err(error) => log(&format!("  sampling failed: {error}")),
                        }
                    }
                }
                let _ = duplication.ReleaseFrame();
                if verdict != Depth::Inconclusive || attempt > 10 {
                    break;
                }
            }
            Err(error) => {
                if attempt == 29 {
                    log(&format!(
                        "  AcquireNextFrame never delivered a frame: {error:?}"
                    ));
                }
            }
        }
    }
    log(&format!("  VERDICT: {}", verdict.label()));
}

/// Ask Windows whether any active output can carry Advanced Color.
///
/// This is the question an HDR EDID exists to change, and it is separate from
/// everything else this probe measures. `IDXGIOutput6::GetDesc1` reports the
/// colour space an output is *currently* in; it does not say whether a wider
/// one is available. Only the display-config path answers that, and it is what
/// would flip if a virtual display began advertising HDR10 in its EDID.
///
/// `advancedColorSupported` is bit 0 of the flags word and
/// `advancedColorEnabled` is bit 1, per `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO`.
unsafe fn report_advanced_color_capability() {
    log("Advanced Color capability (what an HDR EDID would change):");
    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).is_err()
    {
        log("  GetDisplayConfigBufferSizes failed");
        return;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    if QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut path_count,
        paths.as_mut_ptr(),
        &mut mode_count,
        modes.as_mut_ptr(),
        None,
    )
    .is_err()
    {
        log("  QueryDisplayConfig failed");
        return;
    }
    for path in paths.iter().take(path_count as usize) {
        #[repr(C)]
        struct AdvancedColorInfo {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
            flags: u32,
            colour_encoding: i32,
            bits_per_colour_channel: u32,
        }
        let mut info = AdvancedColorInfo {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size: std::mem::size_of::<AdvancedColorInfo>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            flags: 0,
            colour_encoding: 0,
            bits_per_colour_channel: 0,
        };
        let status = DisplayConfigGetDeviceInfo(&mut info.header as *mut _);
        if status != 0 {
            log(&format!(
                "  target {}: query failed ({status})",
                path.targetInfo.id
            ));
            continue;
        }
        let supported = info.flags & 0x1 != 0;
        let enabled = info.flags & 0x2 != 0;
        log(&format!(
            "  target {}: advancedColorSupported={} advancedColorEnabled={} bitsPerColourChannel={}",
            path.targetInfo.id, supported, enabled, info.bits_per_colour_channel
        ));
    }
}

/// Turn Advanced Color on for every target that will accept it.
///
/// Separate from the capability report on purpose. `advancedColorSupported`
/// says Windows *will offer* HDR on an output; the desktop is only actually
/// composited in FP16 scRGB once it is enabled. Until then a wide capture
/// format returns a wide container over 8-bit content, which is the exact
/// false positive this investigation has already produced once.
///
/// Returns the number of targets switched on. Enabling is a real change to the
/// user's display state, so this is opt-in via `color-probe --enable-hdr` and
/// pairs with `--disable-hdr`.
unsafe fn set_advanced_color(enable: bool) -> usize {
    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).is_err()
    {
        log("  could not size the display config");
        return 0;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    if QueryDisplayConfig(
        QDC_ONLY_ACTIVE_PATHS,
        &mut path_count,
        paths.as_mut_ptr(),
        &mut mode_count,
        modes.as_mut_ptr(),
        None,
    )
    .is_err()
    {
        log("  could not query the display config");
        return 0;
    }
    let mut changed = 0usize;
    for path in paths.iter().take(path_count as usize) {
        #[repr(C)]
        struct GetInfo {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
            flags: u32,
            colour_encoding: i32,
            bits_per_colour_channel: u32,
        }
        let mut get = GetInfo {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size: std::mem::size_of::<GetInfo>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            flags: 0,
            colour_encoding: 0,
            bits_per_colour_channel: 0,
        };
        if DisplayConfigGetDeviceInfo(&mut get.header as *mut _) != 0 {
            continue;
        }
        // Only touch outputs that can carry it; asking the rest is noise.
        if get.flags & 0x1 == 0 {
            continue;
        }
        if (get.flags & 0x2 != 0) == enable {
            log(&format!(
                "  target {}: already {}",
                path.targetInfo.id,
                if enable { "enabled" } else { "disabled" }
            ));
            continue;
        }
        #[repr(C)]
        struct SetState {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
            value: u32,
        }
        let mut set = SetState {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
                size: std::mem::size_of::<SetState>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            value: u32::from(enable),
        };
        let status = DisplayConfigSetDeviceInfo(&mut set.header as *mut _);
        if status != 0 {
            log(&format!(
                "  target {}: set advanced colour failed ({status})",
                path.targetInfo.id
            ));
            continue;
        }
        // Read it back rather than trusting the call: a success code that
        // leaves the state unchanged would otherwise read as a working feature.
        let mut verify = GetInfo {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size: std::mem::size_of::<GetInfo>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            flags: 0,
            colour_encoding: 0,
            bits_per_colour_channel: 0,
        };
        let now_enabled = DisplayConfigGetDeviceInfo(&mut verify.header as *mut _) == 0
            && verify.flags & 0x2 != 0;
        log(&format!(
            "  target {}: set advanced colour {} -> readback enabled={} bitsPerColourChannel={}",
            path.targetInfo.id,
            if enable { "ON" } else { "OFF" },
            now_enabled,
            verify.bits_per_colour_channel
        ));
        if now_enabled == enable {
            changed += 1;
        }
    }
    changed
}

/// `capenc color-probe`
pub(crate) unsafe fn run_with(enable_hdr: bool, disable_hdr: bool, wgc_only: bool) {
    if enable_hdr || disable_hdr {
        let want = enable_hdr;
        log(&format!(
            "setting Advanced Color {} on every capable target",
            if want { "ON" } else { "OFF" }
        ));
        let changed = set_advanced_color(want);
        log(&format!("  targets changed: {changed}"));
    }
    run_inner(wgc_only);
}

pub(crate) unsafe fn run_inner(wgc_only: bool) {
    log("Desktop Duplication colour-depth probe");
    report_advanced_color_capability();
    log(
        "NOTE: the 8-bit-grid verdict below is only valid while Advanced Color is OFF. \
         Under HDR, Windows scales SDR content into scRGB and it lands off the grid \
         while carrying no more information.",
    );
    log(&format!(
        "per-monitor DPI awareness: {}",
        if enable_per_monitor_dpi_awareness() {
            "enabled (DuplicateOutput1 requires it)"
        } else {
            "COULD NOT ENABLE - a DXGI_ERROR_UNSUPPORTED below may be this, not the host"
        }
    ));
    log("requesting, widest first: R16G16B16A16_FLOAT, R10G10B10A2_UNORM, B8G8R8A8_UNORM");
    let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
        Ok(factory) => factory,
        Err(error) => {
            log(&format!("CreateDXGIFactory1 failed: {error:?}"));
            return;
        }
    };
    let mut adapter_index = 0u32;
    while let Ok(adapter) = factory.EnumAdapters(adapter_index) {
        let name = adapter
            .GetDesc()
            .map(|d| {
                String::from_utf16_lossy(&d.Description)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_else(|_| "<unknown>".to_string());
        log(&format!("adapter {adapter_index}: {name}"));
        let mut output_index = 0u32;
        while let Ok(output) = adapter.EnumOutputs(output_index) {
            probe_output(&adapter, &output, output_index, wgc_only);
            output_index += 1;
        }
        if output_index == 0 {
            log("  no outputs attached");
        }
        adapter_index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this probe exists to catch: a 10-bit buffer carrying an
    /// 8-bit desktop shifted left by two.
    #[test]
    fn an_eight_bit_source_shifted_into_ten_bits_is_container_only() {
        let pixels: Vec<u32> = (0u32..64)
            .map(|v| {
                let c = (v * 4) << 2;
                c | (c << 10) | (c << 20)
            })
            .collect();
        assert_eq!(
            classify_r10g10b10a2(&pixels),
            Depth::ContainerOnlyZeroFilled
        );
    }

    /// The other widening Windows can do, which zero-fill detection misses.
    #[test]
    fn an_eight_bit_source_bit_replicated_into_ten_bits_is_container_only() {
        let pixels: Vec<u32> = (0u32..64)
            .map(|v| {
                let eight = v * 4;
                let c = (eight << 2) | (eight >> 6);
                c | (c << 10) | (c << 20)
            })
            .collect();
        assert_eq!(
            classify_r10g10b10a2(&pixels),
            Depth::ContainerOnlyBitReplicated
        );
    }

    #[test]
    fn low_bits_that_are_neither_zero_nor_replicated_are_genuine() {
        let pixels: Vec<u32> = (0u32..64)
            .map(|v| {
                // Low bits deliberately uncorrelated with the high bits.
                let c = (v * 13 + 1) & 0x3FF;
                c | (c << 10) | (c << 20)
            })
            .collect();
        assert_eq!(classify_r10g10b10a2(&pixels), Depth::GenuinelyWide);
    }

    /// A black or near-static desktop must not be read as evidence either way.
    #[test]
    fn too_few_distinct_samples_is_inconclusive_not_a_negative() {
        assert_eq!(classify_r10g10b10a2(&[0; 4096]), Depth::Inconclusive);
        assert_eq!(classify_fp16(&[0; 4096]), Depth::Inconclusive);
    }

    /// Half-floats built from exact 8-bit sRGB codes must read as 8-bit
    /// content however many distinct values they contain. The first version of
    /// this classifier counted distinct values and called this "genuine".
    #[test]
    fn eight_bit_srgb_codes_widened_into_fp16_are_not_a_wide_source() {
        let samples: Vec<u16> = (0u16..=255)
            .map(|code| {
                let signal = f32::from(code) / 255.0;
                let linear = if signal <= 0.040_45 {
                    signal / 12.92
                } else {
                    ((signal + 0.055) / 1.055).powf(2.4)
                };
                f32_to_half(linear)
            })
            .collect();
        assert_eq!(
            classify_fp16(&samples),
            Depth::EightBitContentInWideContainer,
            "8-bit codes carried in float must not read as a wide source"
        );
    }

    /// Values deliberately placed between 8-bit grid points.
    #[test]
    fn samples_between_the_eight_bit_grid_points_are_a_wide_source() {
        let samples: Vec<u16> = (0u16..=255)
            .map(|code| {
                let signal = (f32::from(code) + 0.5) / 255.0;
                let linear = if signal <= 0.040_45 {
                    signal / 12.92
                } else {
                    ((signal + 0.055) / 1.055).powf(2.4)
                };
                f32_to_half(linear)
            })
            .collect();
        assert_eq!(classify_fp16(&samples), Depth::GenuinelyWide);
    }

    /// On *uniformly spaced* samples the estimate is scale-invariant, which is
    /// the property that would make it useful under HDR composition.
    #[test]
    fn scaling_uniform_eight_bit_content_does_not_change_the_estimate() {
        for scale in [1.0f32, 0.08, 3.5] {
            let values: Vec<f32> = (0u16..=255).map(|c| f32::from(c) / 255.0 * scale).collect();
            let levels = estimate_levels(&values).expect("levels");
            assert!(
                levels < 400.0,
                "8-bit content scaled by {scale} estimated {levels} levels"
            );
        }
    }

    /// And the reason it cannot be a verdict: scRGB is linear light, and 8-bit
    /// sRGB codes converted to linear are not uniformly spaced, so this reads
    /// them as far finer than 8-bit. Pinned deliberately — if someone makes the
    /// estimator decisive, this test tells them what they still have to solve.
    #[test]
    fn linear_light_from_eight_bit_srgb_defeats_the_level_estimate() {
        let values: Vec<f32> = (0u16..=255)
            .map(|code| {
                let signal = f32::from(code) / 255.0;
                if signal <= 0.040_45 {
                    signal / 12.92
                } else {
                    ((signal + 0.055) / 1.055).powf(2.4)
                }
            })
            .collect();
        let levels = estimate_levels(&values).expect("levels");
        assert!(
            levels > 400.0,
            "expected the linear-light spacing to defeat the estimate, got {levels}"
        );
    }

    /// And the converse must still register.
    #[test]
    fn ten_bit_content_estimates_far_more_levels() {
        for scale in [1.0f32, 0.08, 3.5] {
            let values: Vec<f32> = (0u16..1024)
                .map(|c| f32::from(c) / 1023.0 * scale)
                .collect();
            let levels = estimate_levels(&values).expect("levels");
            assert!(
                levels > 700.0,
                "10-bit content scaled by {scale} estimated only {levels} levels"
            );
        }
    }

    #[test]
    fn a_flat_surface_yields_no_level_estimate() {
        assert!(estimate_levels(&[0.5f32; 4096]).is_none());
    }

    /// Round-trip guard for the decoder the classifier depends on.
    #[test]
    fn half_to_f32_round_trips_representative_values() {
        for value in [0.0f32, 0.25, 0.5, 1.0, 0.003, 0.75] {
            let back = half_to_f32(f32_to_half(value));
            assert!(
                (back - value).abs() < 0.001,
                "half round trip lost {value}: got {back}"
            );
        }
    }

    /// Minimal binary32 -> binary16 for the tests above.
    fn f32_to_half(value: f32) -> u16 {
        let bits = value.to_bits();
        let sign = ((bits >> 31) & 1) as u16;
        let exponent = ((bits >> 23) & 0xFF) as i32;
        let mantissa = bits & 0x7F_FFFF;
        if exponent == 0 {
            return sign << 15;
        }
        let new_exponent = exponent - 127 + 15;
        if new_exponent <= 0 {
            // Subnormal half.
            let shift = (1 - new_exponent) as u32;
            if shift > 24 {
                return sign << 15;
            }
            let m = (mantissa | 0x80_0000) >> (shift + 13);
            return (sign << 15) | (m as u16);
        }
        if new_exponent >= 0x1F {
            return (sign << 15) | (0x1F << 10);
        }
        (sign << 15) | ((new_exponent as u16) << 10) | ((mantissa >> 13) as u16)
    }
}
