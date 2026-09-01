// Windows host implementation: DXGI Desktop Duplication capture +
// (feature "nvenc") NVENC D3D11 zero-copy encode. Split out of main.rs
// when the Linux (NvFBC->CUDA->NVENC) implementation landed; the paced
// encode loop / stdin IDR / stats contract is identical on both.

#[cfg(feature = "nvenc")]
use crate::frame_policy::{choose_frame_action, FrameAction};
use crate::log;

use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Foundation::E_ACCESSDENIED;
#[cfg(feature = "mf")]
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
};
#[cfg(feature = "nvenc")]
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Resource, D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE,
    D3D11_USAGE_STAGING,
};
#[cfg(feature = "nvenc")]
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutput6,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};

/// Explicit adapter/output/device selector for resolving one specific DXGI
/// output, threaded from the pier's freshly re-resolved stable capture
/// binding (see `hosts/windows/src/multi_monitor_topology.rs`'s
/// `CaptureSelector` and `hosts/windows/src/capenc.rs`'s `CapencConfig`).
///
/// `capenc`'s argv cannot select an output by stable adapter LUID directly
/// (there is no `luid=`/`target=` argv contract), so this selector matches on
/// the resolved, human-readable identity the pier already threads through
/// `adapter=`/`adapter-output=`/`device=`: adapter description string,
/// adapter-local output index, and the Win32 `\\.\DISPLAYn` device name. Any
/// field left `None` is not used to narrow candidates; `device_name` alone is
/// enough to resolve uniquely since Windows assigns it per active
/// desktop-attached output system-wide (not scoped to one adapter).
///
/// When every field is `None` — no explicit selector at all, the legacy/
/// standalone invocation shape — resolution falls back to
/// `global_output_index`, the positional attached-output enumeration
/// ordinal, exactly as this crate behaved before an explicit selector
/// existed. Once ANY explicit field is present it takes priority over
/// `global_output_index`: a re-enumeration that shuffles global indices
/// (a monitor unplugged/replugged, or another adapter's output attached)
/// must never silently bind the wrong physical output just because its
/// position in the enumeration order changed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputSelector<'a> {
    pub global_output_index: u32,
    pub adapter_hint: Option<&'a str>,
    pub adapter_output_index: Option<u32>,
    pub device_name: Option<&'a str>,
}

impl OutputSelector<'_> {
    fn has_explicit_selector(&self) -> bool {
        self.adapter_hint.is_some()
            || self.adapter_output_index.is_some()
            || self.device_name.is_some()
    }

    /// Whether one enumerated candidate satisfies every explicit field this
    /// selector actually specifies. Returns `false` when the selector has no
    /// explicit field at all — that case is resolved by `global_output_index`
    /// instead, never by this predicate.
    fn matches(
        &self,
        candidate_adapter_name: &str,
        candidate_local_index: u32,
        candidate_device_name: &str,
    ) -> bool {
        self.has_explicit_selector()
            && self
                .adapter_hint
                .is_none_or(|wanted| candidate_adapter_name.eq_ignore_ascii_case(wanted))
            && self
                .adapter_output_index
                .is_none_or(|wanted| wanted == candidate_local_index)
            && self
                .device_name
                .is_none_or(|wanted| candidate_device_name.eq_ignore_ascii_case(wanted))
    }

    /// Human-readable identity for logs: what the parent asked this child to
    /// bind. Emitted alongside the resolved adapter/output on every backend
    /// so the parent's pre-READY diagnostics (already captured verbatim —
    /// see `hosts/windows/src/capenc.rs::wait_for_ready`) are sufficient to
    /// fail closed on a requested-vs-resolved mismatch without any new IPC.
    pub(crate) fn describe(&self) -> String {
        if self.has_explicit_selector() {
            format!(
                "adapter={} adapter-output={} device={}",
                self.adapter_hint.unwrap_or("<any>"),
                self.adapter_output_index
                    .map_or_else(|| "<any>".to_string(), |value| value.to_string()),
                self.device_name.unwrap_or("<any>"),
            )
        } else {
            format!("output_index={}", self.global_output_index)
        }
    }
}

/// One resolved DXGI adapter+output pair, ready for `DuplicateOutput`/WGC
/// device creation. Every capture backend (DDA, the in-process WGC fallback,
/// and the software H.264 paths) resolves through the same
/// [`resolve_output`] so an `adapter=`/`adapter-output=`/`device=` selector
/// picks the identical DXGI output on every code path — never `output_index`
/// alone once an explicit selector is present.
pub(crate) struct ResolvedOutput {
    pub adapter: IDXGIAdapter,
    pub adapter_name: String,
    pub output: IDXGIOutput,
    pub monitor: HMONITOR,
    pub adapter_index: u32,
    pub vendor_id: u32,
}

/// Decode a NUL-terminated UTF-16 buffer (`WCHAR[N]` Win32 struct fields,
/// e.g. `DXGI_ADAPTER_DESC::Description`/`DXGI_OUTPUT_DESC::DeviceName`) into
/// a `String`, stopping at the first NUL.
fn decode_wide_z(units: &[u16]) -> String {
    units
        .iter()
        .take_while(|&&unit| unit != 0)
        .map(|&unit| char::from_u32(unit as u32).unwrap_or('?'))
        .collect()
}

/// One enumerated desktop-attached output's pure identity fields — no live
/// COM handles — so the actual match/ambiguity decision in
/// [`resolve_selector`] is unit-testable without a real display adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputCandidate {
    attached_global_index: u32,
    adapter_name: String,
    local_index: u32,
    device_name: String,
}

/// Outcome of matching an [`OutputSelector`] against a list of enumerated
/// [`OutputCandidate`]s: which position — if any — resolves, or why it fails
/// closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectorResolution {
    Resolved(usize),
    Missing,
    /// Carries the number of candidates that matched (always `>= 2`).
    Ambiguous(usize),
}

/// Pure resolution core shared by every backend: given the outputs a fresh
/// enumeration found (`candidates`, in enumeration order) and what the pier
/// asked for (`selector`), decide which single candidate — if any — this
/// selector names. All-or-nothing: exactly one candidate must satisfy the
/// selector, or resolution fails closed — either because nothing matched
/// (missing binding, e.g. the monitor was unplugged since the pier last
/// probed) or because more than one candidate matched (ambiguous binding,
/// e.g. two identically named adapters and no `device=` narrowing it
/// further). Never returns a partial/best-effort match.
fn resolve_selector(
    selector: &OutputSelector<'_>,
    candidates: &[OutputCandidate],
) -> SelectorResolution {
    if selector.has_explicit_selector() {
        let mut matches = candidates.iter().enumerate().filter(|(_, candidate)| {
            selector.matches(
                &candidate.adapter_name,
                candidate.local_index,
                &candidate.device_name,
            )
        });
        let Some((first_index, _)) = matches.next() else {
            return SelectorResolution::Missing;
        };
        let extra = matches.count();
        return if extra == 0 {
            SelectorResolution::Resolved(first_index)
        } else {
            SelectorResolution::Ambiguous(extra + 1)
        };
    }
    candidates
        .iter()
        .position(|candidate| candidate.attached_global_index == selector.global_output_index)
        .map_or(SelectorResolution::Missing, SelectorResolution::Resolved)
}

fn unresolved_output_error(
    selector: &OutputSelector<'_>,
    available: &[String],
) -> windows::core::Error {
    windows::core::Error::new(
        E_ACCESSDENIED,
        format!(
            "configured DXGI output not found: {}; available attached outputs: [{}]",
            selector.describe(),
            available.join("; ")
        ),
    )
}

fn ambiguous_output_error(
    selector: &OutputSelector<'_>,
    available: &[String],
    match_count: usize,
) -> windows::core::Error {
    windows::core::Error::new(
        E_ACCESSDENIED,
        format!(
            "configured DXGI output selector is ambiguous ({match_count} outputs matched: {}); \
             available attached outputs: [{}]",
            selector.describe(),
            available.join("; ")
        ),
    )
}

/// One enumerated desktop-attached output: live COM handles plus the pure
/// [`OutputCandidate`] identity `resolve_selector` decides on.
struct EnumeratedOutput {
    adapter: IDXGIAdapter,
    output: IDXGIOutput,
    monitor: HMONITOR,
    adapter_index: u32,
    vendor_id: u32,
    identity: OutputCandidate,
}

/// Enumerate every DXGI adapter and its desktop-attached outputs. Called
/// fresh on every `resolve_output` invocation — i.e. fresh on every process
/// start/restart of this child — so a re-enumeration between the pier's own
/// probe and this child spawning always sees the CURRENT hardware state, not
/// a cached one.
unsafe fn enumerate_attached_outputs() -> windows::core::Result<Vec<EnumeratedOutput>> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
    let mut outputs = Vec::new();
    let mut attached_global_index = 0u32;
    let mut ai = 0u32;
    loop {
        let adapter: IDXGIAdapter = match factory.EnumAdapters(ai) {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        let adapter_desc = adapter.GetDesc().unwrap_or_default();
        let adapter_name = decode_wide_z(&adapter_desc.Description);
        let mut oi = 0u32;
        loop {
            let output = match adapter.EnumOutputs(oi) {
                Ok(output) => output,
                Err(_) => break,
            };
            let local_index = oi;
            oi += 1;
            let output_desc = match output.GetDesc() {
                Ok(desc) => desc,
                Err(error) => {
                    log(&format!(
                        "adapter {ai} output {local_index} GetDesc failed: {error:?}"
                    ));
                    continue;
                }
            };
            if !output_desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let device_name = decode_wide_z(&output_desc.DeviceName);
            outputs.push(EnumeratedOutput {
                adapter: adapter.clone(),
                output,
                monitor: output_desc.Monitor,
                adapter_index: ai,
                vendor_id: adapter_desc.VendorId,
                identity: OutputCandidate {
                    attached_global_index,
                    adapter_name: adapter_name.clone(),
                    local_index,
                    device_name,
                },
            });
            attached_global_index += 1;
        }
        ai += 1;
    }
    Ok(outputs)
}

/// Enumerate every DXGI adapter + desktop-attached output and resolve
/// `selector` against it via [`resolve_selector`]. See that function's
/// documentation for the all-or-nothing fail-closed contract.
pub(crate) unsafe fn resolve_output(
    selector: &OutputSelector<'_>,
) -> windows::core::Result<ResolvedOutput> {
    let outputs = enumerate_attached_outputs()?;
    let candidates: Vec<OutputCandidate> =
        outputs.iter().map(|found| found.identity.clone()).collect();
    let available: Vec<String> = candidates
        .iter()
        .map(|candidate| {
            format!(
                "global={} adapter={:?} adapter_output={} device={}",
                candidate.attached_global_index,
                candidate.adapter_name,
                candidate.local_index,
                candidate.device_name
            )
        })
        .collect();
    match resolve_selector(selector, &candidates) {
        SelectorResolution::Resolved(index) => {
            let found = outputs
                .into_iter()
                .nth(index)
                .expect("resolved index is within the enumerated candidates");
            Ok(ResolvedOutput {
                adapter_name: found.identity.adapter_name,
                adapter: found.adapter,
                output: found.output,
                monitor: found.monitor,
                adapter_index: found.adapter_index,
                vendor_id: found.vendor_id,
            })
        }
        SelectorResolution::Missing => Err(unresolved_output_error(selector, &available)),
        SelectorResolution::Ambiguous(count) => {
            Err(ambiguous_output_error(selector, &available, count))
        }
    }
}

/// Result of [`Capture::find_output_device`]: the resolved DXGI output plus
/// the D3D11 device/context created on the SAME adapter (required for
/// `DuplicateOutput`), and enough resolved identity (`adapter_name`,
/// `adapter_index`, `vendor_id`) for callers to log which physical output
/// and adapter this child actually bound.
struct FoundOutputDevice {
    output1: IDXGIOutput1,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    monitor: HMONITOR,
    adapter_index: u32,
    vendor_id: u32,
    adapter_name: String,
}

struct Capture {
    _device: ID3D11Device,
    _context: ID3D11DeviceContext,
    output1: IDXGIOutput1,
    dupl: IDXGIOutputDuplication,
    width: u32,
    height: u32,
}

impl Capture {
    /// Find the adapter that OWNS the selected desktop-attached output,
    /// create the D3D11 device ON THAT ADAPTER, and duplicate that output.
    ///
    /// The naive `D3D11CreateDevice(default adapter)` + `EnumOutputs(0)` fails
    /// on multi-GPU hosts (here: V100 + RTX6000 + virtio): DuplicateOutput
    /// succeeds but AcquireNextFrame times out FOREVER because the desktop is
    /// composited on a different adapter. Desktop Duplication requires the
    /// device and the output to be on the same adapter. `selector` picks the
    /// exact output — either by `global_output_index` (0 = primary, the
    /// legacy/standalone shape) or by an explicit adapter/output/device
    /// binding resolved fresh via [`resolve_output`].
    unsafe fn new(selector: &OutputSelector<'_>) -> windows::core::Result<Self> {
        let found = Self::find_output_device(selector)?;
        let (dupl, width, height) = Self::duplicate(&found.output1, &found.device)?;
        log(&format!(
            "bound {} -> adapter {} ({:?})",
            selector.describe(),
            found.adapter_index,
            found.adapter_name
        ));
        Ok(Self {
            _device: found.device,
            _context: found.context,
            output1: found.output1,
            dupl,
            width,
            height,
        })
    }

    unsafe fn find_output_device(
        selector: &OutputSelector<'_>,
    ) -> windows::core::Result<FoundOutputDevice> {
        let resolved = resolve_output(selector)?;
        let output1: IDXGIOutput1 = resolved.output.cast()?;
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &resolved.adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        Ok(FoundOutputDevice {
            output1,
            device: device.expect("D3D11 device"),
            context: context.expect("D3D11 context"),
            monitor: resolved.monitor,
            adapter_index: resolved.adapter_index,
            vendor_id: resolved.vendor_id,
            adapter_name: resolved.adapter_name,
        })
    }

    unsafe fn duplicate(
        output1: &IDXGIOutput1,
        device: &ID3D11Device,
    ) -> windows::core::Result<(IDXGIOutputDuplication, u32, u32)> {
        let dupl = output1.DuplicateOutput(device)?;
        // windows-rs: IDXGIOutputDuplication::GetDesc() RETURNS the desc.
        let desc = dupl.GetDesc();
        let w = desc.ModeDesc.Width;
        let h = desc.ModeDesc.Height;
        Ok((dupl, w, h))
    }

    /// Re-establish the output duplication after `ACCESS_LOST` (mode change /
    /// secure desktop / lock / fast-user-switch). `DuplicateOutput` ITSELF
    /// transiently fails while the desktop is mid-transition — the display
    /// needs ~200–500 ms to settle after a modeset — so a single call that
    /// propagates its error with `?` would kill the whole helper on an event
    /// we are expected to survive. Retry with a short backoff instead. Honest
    /// logging: report how long recovery took and whether the geometry changed
    /// (a geometry change means the fixed-size NVENC encoder is now mismatched;
    /// dynamic re-init is the documented Track-1 follow-up — for the current
    /// bring-up the head is sized once at startup so steady-state recovery is
    /// same-geometry).
    unsafe fn reduplicate(&mut self) -> windows::core::Result<()> {
        let (prev_w, prev_h) = (self.width, self.height);
        let mut last_err: Option<windows::core::Error> = None;
        for attempt in 1..=25u32 {
            // Up to ~5 s total; the display settle window is 200–500 ms.
            std::thread::sleep(std::time::Duration::from_millis(200));
            match Self::duplicate(&self.output1, &self._device) {
                Ok((dupl, w, h)) => {
                    self.dupl = dupl;
                    self.width = w;
                    self.height = h;
                    if (w, h) != (prev_w, prev_h) {
                        log(&format!(
                            "duplication re-established at {w}x{h} (was {prev_w}x{prev_h}) \
                             after {attempt} attempt(s) — GEOMETRY CHANGED, encoder now \
                             mismatched until helper restart"
                        ));
                    } else {
                        log(&format!(
                            "duplication re-established ({w}x{h}) after {attempt} attempt(s)"
                        ));
                    }
                    return Ok(());
                }
                Err(e) => last_err = Some(e),
            }
        }
        log("duplication recovery FAILED after 25 attempts (~5s)");
        Err(last_err.unwrap_or_else(|| {
            windows::core::Error::new(E_ACCESSDENIED, "reduplicate exhausted retries")
        }))
    }

    /// Acquire one frame. Returns Some(texture) on a new frame, None on timeout
    /// (no change). Recreates the duplication on AccessLost (modeset / secure
    /// desktop / session switch). `dbg` counts outcome categories. Used by the
    /// capture-only build; the NVENC build stages inside `acquire_into`.
    #[cfg(not(feature = "nvenc"))]
    unsafe fn acquire(
        &mut self,
        timeout_ms: u32,
        dbg: &mut (u64, u64, u64),
    ) -> windows::core::Result<Option<ID3D11Texture2D>> {
        let mut info: DXGI_OUTDUPL_FRAME_INFO = Default::default();
        let mut resource: Option<IDXGIResource> = None;
        match self
            .dupl
            .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        {
            Ok(()) => {
                let tex: ID3D11Texture2D = resource.unwrap().cast()?;
                let new_image = info.AccumulatedFrames > 0 || info.LastPresentTime != 0;
                if new_image {
                    dbg.0 += 1;
                } else {
                    dbg.2 += 1;
                } // new / cursor-only
                self.dupl.ReleaseFrame()?;
                Ok(if new_image { Some(tex) } else { None })
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                dbg.1 += 1; // timeout
                Ok(None)
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                log("AccessLost — recreating duplication (settle + retry)");
                self.reduplicate()?;
                Ok(None)
            }
            Err(e) if e.code() == E_ACCESSDENIED => {
                // Session 0 / no desktop access.
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(feature = "nvenc")]
    fn device(&self) -> &ID3D11Device {
        &self._device
    }
    #[cfg(feature = "nvenc")]
    fn context(&self) -> &ID3D11DeviceContext {
        &self._context
    }

    /// Acquire one frame and, if it's a NEW desktop image, hand it to `on_new`
    /// BEFORE `ReleaseFrame` (so the callback can CopyResource the still-valid
    /// surface). Returns true if a new image was staged. Recreates on
    /// AccessLost. Used by the NVENC encode loop.
    #[cfg(feature = "nvenc")]
    unsafe fn acquire_into(
        &mut self,
        timeout_ms: u32,
        dbg: &mut (u64, u64, u64),
        mut on_new: impl FnMut(&ID3D11Texture2D),
    ) -> windows::core::Result<bool> {
        let mut info: DXGI_OUTDUPL_FRAME_INFO = Default::default();
        let mut resource: Option<IDXGIResource> = None;
        match self
            .dupl
            .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        {
            Ok(()) => {
                let new_image = info.AccumulatedFrames > 0 || info.LastPresentTime != 0;
                if new_image {
                    dbg.0 += 1;
                    let tex: ID3D11Texture2D = resource.unwrap().cast()?;
                    on_new(&tex); // stage (GPU copy) while the frame is still held
                } else {
                    dbg.2 += 1;
                }
                self.dupl.ReleaseFrame()?;
                Ok(new_image)
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                dbg.1 += 1;
                Ok(false)
            }
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                log("AccessLost — recreating duplication (settle + retry)");
                self.reduplicate()?;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

/// Capture backend selected at runtime. DXGI Desktop Duplication is the primary
/// (lowest latency on bare metal); WGC is the fallback that works on headless
/// NVIDIA vGPU / RDP consoles where DD delivers no frames.
#[cfg(feature = "nvenc")]
enum Source {
    Dda {
        capture: Capture,
        primed: Option<ID3D11Texture2D>,
    },
    Wgc(crate::wgc::WgcCapture),
}

#[cfg(feature = "nvenc")]
impl Source {
    /// Which capture path this source is, for the READY line.
    ///
    /// Derived from the variant that actually won the probe rather than from
    /// what was configured: the DDA -> WGC fallback is silent, and on a
    /// headless vGPU WGC is what really runs.
    /// Whether this source delivers FP16 scRGB samples.
    ///
    /// DDA has no wide path here -- `DuplicateOutput1` returns BGRA8 whatever
    /// format list it is given -- so only a WGC pool that was actually granted
    /// FP16 answers true.
    fn is_wide(&self) -> bool {
        match self {
            Source::Dda { .. } => false,
            Source::Wgc(w) => w.is_wide(),
        }
    }

    fn capture_backend(&self) -> arcen_media::video::CaptureBackend {
        match self {
            Source::Dda { .. } => arcen_media::video::CaptureBackend::DesktopDuplication,
            Source::Wgc(_) => arcen_media::video::CaptureBackend::WindowsGraphicsCapture,
        }
    }

    fn device(&self) -> &ID3D11Device {
        match self {
            Source::Dda { capture, .. } => capture.device(),
            Source::Wgc(w) => w.device(),
        }
    }
    fn context(&self) -> &ID3D11DeviceContext {
        match self {
            Source::Dda { capture, .. } => capture.context(),
            Source::Wgc(w) => w.context(),
        }
    }
    fn width(&self) -> u32 {
        match self {
            Source::Dda { capture, .. } => capture.width,
            Source::Wgc(w) => w.width,
        }
    }
    fn height(&self) -> u32 {
        match self {
            Source::Dda { capture, .. } => capture.height,
            Source::Wgc(w) => w.height,
        }
    }
    unsafe fn acquire_into(
        &mut self,
        timeout_ms: u32,
        dbg: &mut (u64, u64, u64),
        mut on_new: impl FnMut(&ID3D11Texture2D),
    ) -> windows::core::Result<bool> {
        match self {
            Source::Dda { capture, primed } => {
                if let Some(texture) = primed.take() {
                    dbg.0 += 1;
                    on_new(&texture);
                    Ok(true)
                } else {
                    capture.acquire_into(timeout_ms, dbg, on_new)
                }
            }
            Source::Wgc(w) => w.acquire_into(dbg, &mut on_new),
        }
    }
}

#[cfg(feature = "nvenc")]
unsafe fn retain_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
) -> windows::core::Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    source.GetDesc(&mut desc);
    let mut retained = None;
    device.CreateTexture2D(&desc, None, Some(&mut retained))?;
    let retained = retained.expect("retained probe texture");
    let src: ID3D11Resource = source.cast()?;
    let dst: ID3D11Resource = retained.cast()?;
    context.CopyResource(&dst, &src);
    Ok(retained)
}

/// Pick the live capture backend. Order: forced flag > DXGI DD (probed for
/// actual desktop-image delivery) > WGC fallback. The probe is what makes this
/// robust on headless vGPU: DD `Capture::new` succeeds and can even deliver
/// cursor-only metadata, but no desktop presents. Cursor-only frames do not
/// prove that usable pixels flow, so only a real image accepts the DDA path.
#[cfg(feature = "nvenc")]
const fn wide_capture_required(bit_depth: arcen_media::BitDepth) -> bool {
    !matches!(bit_depth, arcen_media::BitDepth::Eight)
}

#[cfg(feature = "nvenc")]
const fn hdr_output_required(transfer: arcen_media::TransferCharacteristics) -> bool {
    matches!(transfer, arcen_media::TransferCharacteristics::Pq)
}

#[cfg(feature = "nvenc")]
fn wgc_requirement(
    force_wgc: bool,
    cursor_mode: crate::CursorCaptureMode,
    wide_capture: bool,
) -> Option<&'static str> {
    if wide_capture {
        Some("capture backend: WGC (required by FP16 wide capture)")
    } else if cursor_mode.requires_wgc() {
        Some("capture backend: WGC (required by host cursor mode)")
    } else if force_wgc {
        Some("capture backend: WGC (forced via 'wgc' arg)")
    } else {
        None
    }
}

#[cfg(feature = "nvenc")]
/// `wide_capture` asks the WGC path for an FP16 scRGB pool. Without it the
/// desktop arrives as 8-bit BGRA no matter what bit depth the stream is
/// signalled as. `hdr_required` is stricter: it additionally proves that the
/// selected output is actually compositing PQ/BT.2020 before capture starts.
unsafe fn select_source(
    selector: &OutputSelector<'_>,
    force_wgc: bool,
    force_dda: bool,
    cursor_mode: crate::CursorCaptureMode,
    wide_capture: bool,
    hdr_required: bool,
) -> Source {
    if force_dda && wide_capture {
        log("ERROR: Desktop Duplication cannot serve an FP16 wide capture");
        std::process::exit(2);
    }
    if let Some(reason) = wgc_requirement(force_wgc, cursor_mode, wide_capture) {
        log(reason);
        return build_wgc(selector, cursor_mode, wide_capture, hdr_required);
    }
    match Capture::new(selector) {
        Ok(mut cap) => {
            log(&format!(
                "DXGI ready: {}x{} {} — probing frame delivery",
                cap.width,
                cap.height,
                selector.describe()
            ));
            if force_dda {
                log("capture backend: DXGI Desktop Duplication (forced via 'ddapi' arg)");
                return Source::Dda {
                    capture: cap,
                    primed: None,
                };
            }
            let device = cap.device().clone();
            let context = cap.context().clone();
            let mut primed = None;
            let mut retain_error = None;
            let mut dbg = (0u64, 0u64, 0u64);
            let start = Instant::now();
            while start.elapsed().as_secs_f64() < 1.5 {
                let _ = cap.acquire_into(50, &mut dbg, |texture| {
                    match retain_texture(&device, &context, texture) {
                        Ok(texture) => primed = Some(texture),
                        Err(error) => retain_error = Some(error),
                    }
                });
                if let Some(error) = retain_error.take() {
                    log(&format!("failed to retain DDA probe frame: {error:?}"));
                    break;
                }
                if primed.is_some() {
                    log(&format!(
                        "capture backend: DXGI Desktop Duplication (delivered new={} cursor={})",
                        dbg.0, dbg.2
                    ));
                    return Source::Dda {
                        capture: cap,
                        primed,
                    };
                }
            }
            log(&format!(
                "DXGI delivered 0 desktop images in 1.5s (headless vGPU/RDP?) — falling back to WGC (timeouts={} cursor_only={})",
                dbg.1, dbg.2
            ));
            drop(cap);
            build_wgc(selector, cursor_mode, wide_capture, hdr_required)
        }
        Err(e) => {
            log(&format!("DXGI init failed ({e:?}) — falling back to WGC"));
            build_wgc(selector, cursor_mode, wide_capture, hdr_required)
        }
    }
}

#[cfg(feature = "nvenc")]
unsafe fn build_wgc(
    selector: &OutputSelector<'_>,
    cursor_mode: crate::CursorCaptureMode,
    wide_capture: bool,
    hdr_required: bool,
) -> Source {
    let found = match Capture::find_output_device(selector) {
        Ok(found) => found,
        Err(e) => {
            log(&format!("WGC device init failed: {e:?}"));
            std::process::exit(2);
        }
    };
    log(&format!(
        "WGC bound {} -> adapter {} ({:?})",
        selector.describe(),
        found.adapter_index,
        found.adapter_name
    ));
    let output_colour = match found.output1.cast::<IDXGIOutput6>() {
        Ok(output) => match output.GetDesc1() {
            Ok(desc) => {
                log(&format!(
                    "WGC output colour: bits_per_color={} color_space={:?} min_luminance={} \
                     max_luminance={} max_full_frame_luminance={}",
                    desc.BitsPerColor,
                    desc.ColorSpace,
                    desc.MinLuminance,
                    desc.MaxLuminance,
                    desc.MaxFullFrameLuminance
                ));
                Some(desc.ColorSpace)
            }
            Err(error) => {
                log(&format!("WGC output colour query failed: {error:?}"));
                None
            }
        },
        Err(error) => {
            log(&format!(
                "WGC output has no IDXGIOutput6 colour state: {error:?}"
            ));
            None
        }
    };
    if hdr_required && output_colour != Some(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020) {
        log(&format!(
            "ERROR: HDR capture requires DXGI RGB_FULL_G2084_NONE_P2020 output, got \
             {output_colour:?}"
        ));
        std::process::exit(2);
    }
    match crate::wgc::WgcCapture::from_device(
        found.device,
        found.context,
        found.monitor,
        cursor_mode,
        wide_capture,
    ) {
        Ok(w) => Source::Wgc(w),
        Err(e) => {
            log(&format!("WGC init failed: {e:?}"));
            std::process::exit(2);
        }
    }
}

/// Stage 2: fused DXGI capture + NVENC D3D11 zero-copy encode. Paces at the
/// target fps, re-encoding the last staged frame when the desktop is static
/// (small P-frames) so the stream never stalls. Writes **length-prefixed**
/// access units to stdout — each AU is `[u32 LE byte-length][Annex-B bytes]`.
/// The Python parent (`capenc_backend._read_stream_lp`) reads the 4-byte
/// length then exactly that many bytes, so one AU maps to exactly one wire
/// frame regardless of how the OS pipe chunks the reads. This replaces the
/// old raw-Annex-B + start-code-splitter contract, whose "short read = AU
/// boundary" heuristic mis-fired on Windows' small unbuffered pipe reads and
/// split a single large keyframe across many WebSocket messages (the decoder
/// then saw truncated NALs and stalled on a black frame).
#[cfg(feature = "nvenc")]
unsafe fn create_nvenc_encoder(
    cap: &Source,
    codec: &str,
    color: crate::ColorSpec,
    intent: arcen_media::EncodeIntent,
    qp_map_policy: crate::qp_map::QpMapPolicy,
) -> Result<crate::nvenc::Encoder, crate::nvenc::NvencInitError> {
    crate::nvenc::Encoder::new(
        cap.device(),
        cap.context(),
        cap.width(),
        cap.height(),
        codec,
        color,
        intent,
        qp_map_policy,
        // From the pool that was actually created, so staging and conversion
        // follow the concrete source format rather than the request.
        cap.is_wide(),
    )
}

#[cfg(feature = "nvenc")]
unsafe fn run_encode(
    mut cap: Source,
    mut encoder: crate::nvenc::Encoder,
    codec: &str,
    fps: u32,
    color: crate::ColorSpec,
    framed: bool,
    cursor_mode: crate::CursorCaptureMode,
) -> i32 {
    log(&format!(
        "NVENC ready: {}x{} codec={} chroma={:?} depth={:?} range={:?} matrix={:?} \
         primaries={:?} transfer={:?} capture_format={}",
        cap.width(),
        cap.height(),
        codec,
        color.chroma,
        color.bit_depth,
        color.range,
        color.matrix,
        color.primaries,
        color.transfer,
        if cap.is_wide() {
            "R16G16B16A16Float"
        } else {
            "B8G8R8A8UIntNormalized"
        }
    ));
    // Captured before the loop borrows `cap` mutably; the variant cannot
    // change once the source is built.
    let announced_capture = cap.capture_backend();
    // The exact same `color` this run's `Encoder` was constructed with (see
    // `create_nvenc_encoder`'s caller) — never a separately re-derived
    // `ColorSpec::legacy(...)` — so the READY line this builds and the
    // stream the encoder actually produces cannot disagree.
    let ready_plan = match crate::resolved_media_plan(
        arcen_media::video::EncoderBackend::NativeNvenc,
        codec,
        color,
        cap.width(),
        cap.height(),
        fps,
        cursor_mode,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            log(&error);
            return 4;
        }
    };

    let control = crate::spawn_control_thread("NVENC");

    let mut stdout = std::io::stdout();
    let target_dt = crate::frame_interval_from_fps(fps);
    let started = Instant::now();
    let mut next = Instant::now();
    let mut first = true; // first encoded picture is a forced IDR
    let mut have_frame = false;
    let mut have_latest = false;
    // A published frame that has not yet been submitted for encode. The
    // current slot still holds it, because `write_idx` only advances inside
    // `Encoder::encode`.
    let mut fresh_pending = false;
    let mut announced_black = false;

    let mut dbg = (0u64, 0u64, 0u64); // new, timeout, cursor-only
    let mut sec = Instant::now();
    let mut enc_count = 0u64;
    let mut enc_ms_sum = 0.0f64;
    let mut enc_ms_max = 0.0f64;
    let mut stage_count = 0u64;
    let mut stage_ms_sum = 0.0f64;
    let mut stage_ms_max = 0.0f64;
    let mut dxgi_copy_ms_sum = 0.0f64;
    let mut dxgi_copy_ms_max = 0.0f64;
    let mut readback_ms_sum = 0.0f64;
    let mut readback_ms_max = 0.0f64;
    let mut conversion_ms_sum = 0.0f64;
    let mut conversion_ms_max = 0.0f64;
    let mut mirror_ms_sum = 0.0f64;
    let mut mirror_ms_max = 0.0f64;
    // Encode submissions bucketed by the frame action that produced them.
    //
    // `avg_encode_ms` mixes a freshly captured and converted frame with a
    // republished still frame and a blank startup submission, which are three
    // different costs. They are kept separate here so a hardware run can tell
    // "encoding hard motion got slower" from "the ring is idling", and the
    // legacy aggregate is still emitted unchanged for compatibility.
    let mut fresh_encode_count = 0u64;
    let mut fresh_encode_ms_sum = 0.0f64;
    let mut fresh_encode_ms_max = 0.0f64;
    let mut restaged_encode_count = 0u64;
    let mut restaged_encode_ms_sum = 0.0f64;
    let mut restaged_encode_ms_max = 0.0f64;
    let mut blank_encode_count = 0u64;
    let mut blank_encode_ms_sum = 0.0f64;
    let mut blank_encode_ms_max = 0.0f64;
    // Cost of the `latest -> next input slot` republish itself, which is a
    // whole-frame host memcpy and not part of any encode call.
    let mut restage_ms_sum = 0.0f64;
    let mut restage_ms_max = 0.0f64;
    let mut restage_copied = 0u64;
    let mut restage_skipped = 0u64;
    let mut restage_unavailable = 0u64;
    let mut bytes_sum = 0u64;
    let mut encode_submitted = 0u64;
    let mut encode_skipped_no_new = 0u64;
    let mut ready_announced = false;

    while !control.stop_requested() {
        // Drain toward the newest frame (short timeout keeps the pace tight and
        // capture latency low). Only the GPU CopyResource runs while the DXGI
        // frame is held; the CPU Map, colour conversion, NVENC write and
        // mirror copy all run after ReleaseFrame, so Desktop Duplication can
        // accumulate the next frame while this one is still being converted.
        let mut copy_error = None;
        let mut copy_ms = 0.0f64;
        let new_frame = match cap.acquire_into(2, &mut dbg, |tex| {
            let copy_started = Instant::now();
            let copied = encoder.copy_acquired_texture(tex);
            copy_ms = copy_started.elapsed().as_secs_f64() * 1000.0;
            if let Err(e) = copied {
                copy_error = Some(e);
            }
        }) {
            Ok(new_frame) => new_frame,
            Err(e) => {
                log(&format!("acquire error: {e:?}"));
                return 3;
            }
        };
        if let Some(e) = copy_error {
            log(&format!("stage failed: {e}"));
            return 3;
        }
        // The DXGI frame is released by this point.
        if new_frame {
            let publish_started = Instant::now();
            let published = encoder.convert_and_publish_staging();
            let stage_ms = copy_ms + publish_started.elapsed().as_secs_f64() * 1000.0;
            stage_count += 1;
            stage_ms_sum += stage_ms;
            stage_ms_max = stage_ms_max.max(stage_ms);
            match published {
                Ok(()) => {
                    let timing = encoder.stage_timing();
                    dxgi_copy_ms_sum += timing.copy_ms;
                    dxgi_copy_ms_max = dxgi_copy_ms_max.max(timing.copy_ms);
                    readback_ms_sum += timing.readback_ms;
                    readback_ms_max = readback_ms_max.max(timing.readback_ms);
                    conversion_ms_sum += timing.conversion_ms;
                    conversion_ms_max = conversion_ms_max.max(timing.conversion_ms);
                    mirror_ms_sum += timing.mirror_ms;
                    mirror_ms_max = mirror_ms_max.max(timing.mirror_ms);
                    have_frame = true;
                    have_latest = true;
                    // The encode deadline is usually still in the future when
                    // a capture lands: the loop polls DXGI every ~2 ms while
                    // the ring only submits every ~33 ms. Without carrying
                    // this across iterations, a genuinely fresh frame would be
                    // classified as a restage simply because the poll that
                    // happened to coincide with the deadline was empty, and
                    // `avg_fresh_encode_ms` would measure almost nothing.
                    fresh_pending = true;
                }
                Err(e) => {
                    // A failed publish must not announce a new frame: the ring
                    // keeps republishing the last frame that did convert.
                    log(&format!("stage failed: {e}"));
                    return 3;
                }
            }
        }
        // Serve-black-until-content parity with the Python capture backend:
        // if no desktop present arrives within the grace window (headless VM,
        // display waking up), start encoding the blank input texture anyway so
        // the client always gets a stream. Real content takes over the moment
        // DXGI delivers a frame.
        if !have_frame && started.elapsed().as_millis() >= 1000 {
            have_frame = true;
            announced_black = true;
            log("no desktop frame after 1s — streaming blank frames until content arrives");
        }

        let now = Instant::now();
        if now >= next {
            let action = choose_frame_action(new_frame || fresh_pending, have_latest, have_frame);
            match action {
                FrameAction::NewFrameStaged | FrameAction::SubmitBlank => {}
                FrameAction::RestageLatest => {
                    let restage_started = Instant::now();
                    let restaged = encoder.restage_latest();
                    let restage_ms = restage_started.elapsed().as_secs_f64() * 1000.0;
                    restage_ms_sum += restage_ms;
                    restage_ms_max = restage_ms_max.max(restage_ms);
                    match restaged {
                        Ok(outcome) => {
                            match outcome {
                                crate::nvenc::RestageOutcome::Copied => restage_copied += 1,
                                // The slot already held exactly this frame, so
                                // the whole-frame copy was skipped. Counted
                                // separately: it means less memory traffic on
                                // a static desktop, never that a new frame was
                                // captured.
                                crate::nvenc::RestageOutcome::AlreadyCurrent => {
                                    restage_skipped += 1;
                                }
                                crate::nvenc::RestageOutcome::NoLatest => {}
                            }
                            if !outcome.is_staged() {
                                // The frame policy only asks for a restage
                                // once a frame has been published, so this is
                                // a disagreement between the loop and the
                                // encoder, not an ordinary idle frame. The
                                // slot is submitted unchanged, exactly as
                                // before, but the condition is counted instead
                                // of being invisible.
                                restage_unavailable += 1;
                            }
                        }
                        Err(e) => {
                            log(&format!("restage latest failed: {e}"));
                            return 3;
                        }
                    }
                }
                FrameAction::SkipNoNew => {
                    encode_skipped_no_new += 1;
                    next += target_dt;
                    if next < now {
                        next = now + target_dt;
                    }
                    continue;
                }
            }
            if announced_black && dbg.0 > 0 {
                announced_black = false;
                log("live desktop content flowing");
            }
            let force = first || control.take_idr();
            if force && !first {
                log("consuming IDR request");
            }
            let t0 = Instant::now();
            encode_submitted += 1;
            let encoded = encoder.encode(force);
            // Timed once, for every submission, and attributed to the action
            // that produced it — including submissions NVENC accepted without
            // returning an access unit, which the legacy aggregate below never
            // counted at all.
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            // The slot has been submitted, so whatever it held is no longer
            // pending regardless of how the submission went.
            fresh_pending = false;
            match action {
                FrameAction::NewFrameStaged => {
                    fresh_encode_count += 1;
                    fresh_encode_ms_sum += ms;
                    fresh_encode_ms_max = fresh_encode_ms_max.max(ms);
                }
                FrameAction::RestageLatest => {
                    restaged_encode_count += 1;
                    restaged_encode_ms_sum += ms;
                    restaged_encode_ms_max = restaged_encode_ms_max.max(ms);
                }
                FrameAction::SubmitBlank => {
                    blank_encode_count += 1;
                    blank_encode_ms_sum += ms;
                    blank_encode_ms_max = blank_encode_ms_max.max(ms);
                }
                // `continue`d above; kept exhaustive so a new action cannot
                // silently fall out of the split.
                FrameAction::SkipNoNew => {}
            }
            match encoded {
                Ok(out) => {
                    first = false; // the forced IDR rode on the submitted frame
                    if let Some(au) = out {
                        if !ready_announced {
                            if au.is_empty()
                                || au.len() > crate::MAX_ACCESS_UNIT_BYTES
                                || crate::announce_ready_from(ready_plan, Some(announced_capture))
                                    .is_err()
                            {
                                log("could not emit READY after first in-memory NVENC access unit");
                                return 5;
                            }
                            ready_announced = true;
                        }
                        // Parent gone / pipe closed -> exit cleanly.
                        if crate::write_access_unit(&mut stdout, &au, framed).is_err() {
                            return 0;
                        }
                        enc_count += 1;
                        enc_ms_sum += ms;
                        if ms > enc_ms_max {
                            enc_ms_max = ms;
                        }
                        bytes_sum += au.len() as u64;
                    }
                }
                Err(e) => {
                    log(&format!("encode error: {e}"));
                    return 5;
                }
            }
            next += target_dt;
            if next < now {
                next = now + target_dt; // we fell behind; resync the schedule
            }
        }

        if sec.elapsed().as_secs_f64() >= 1.0 {
            let avg = if enc_count > 0 {
                enc_ms_sum / enc_count as f64
            } else {
                0.0
            };
            let stage_avg = if stage_count > 0 {
                stage_ms_sum / stage_count as f64
            } else {
                0.0
            };
            let readback_avg = if stage_count > 0 {
                readback_ms_sum / stage_count as f64
            } else {
                0.0
            };
            let copy_avg = if stage_count > 0 {
                dxgi_copy_ms_sum / stage_count as f64
            } else {
                0.0
            };
            let conversion_avg = if stage_count > 0 {
                conversion_ms_sum / stage_count as f64
            } else {
                0.0
            };
            let mirror_avg = if stage_count > 0 {
                mirror_ms_sum / stage_count as f64
            } else {
                0.0
            };
            let mean = |sum: f64, count: u64| if count > 0 { sum / count as f64 } else { 0.0 };
            log(&format!(
                "enc_fps={} cap_fps={} avg_stage_ms={:.2} max_stage_ms={:.2} \
                 avg_copy_ms={:.2} max_copy_ms={:.2} \
                 avg_readback_ms={:.2} max_readback_ms={:.2} avg_conversion_ms={:.2} \
                 max_conversion_ms={:.2} avg_mirror_ms={:.2} max_mirror_ms={:.2} \
                 avg_encode_ms={:.2} max_encode_ms={:.2} \
                 fresh_encode_count={} avg_fresh_encode_ms={:.2} max_fresh_encode_ms={:.2} \
                 restaged_encode_count={} avg_restaged_encode_ms={:.2} \
                 max_restaged_encode_ms={:.2} blank_encode_count={} \
                 avg_blank_encode_ms={:.2} max_blank_encode_ms={:.2} \
                 avg_restage_ms={:.2} max_restage_ms={:.2} \
                 restage_copied={} restage_skipped={} restage_unavailable={} kbps={} \
                 capture_new={} capture_empty={} encode_submitted={} \
                 encode_skipped_no_new={} timeout={} cursor_only={} want_idr={}",
                enc_count,
                dbg.0,
                stage_avg,
                stage_ms_max,
                copy_avg,
                dxgi_copy_ms_max,
                readback_avg,
                readback_ms_max,
                conversion_avg,
                conversion_ms_max,
                mirror_avg,
                mirror_ms_max,
                avg,
                enc_ms_max,
                fresh_encode_count,
                mean(fresh_encode_ms_sum, fresh_encode_count),
                fresh_encode_ms_max,
                restaged_encode_count,
                mean(restaged_encode_ms_sum, restaged_encode_count),
                restaged_encode_ms_max,
                blank_encode_count,
                mean(blank_encode_ms_sum, blank_encode_count),
                blank_encode_ms_max,
                mean(restage_ms_sum, restaged_encode_count),
                restage_ms_max,
                restage_copied,
                restage_skipped,
                restage_unavailable,
                bytes_sum * 8 / 1000,
                dbg.0,
                dbg.1 + dbg.2,
                encode_submitted,
                encode_skipped_no_new,
                dbg.1,
                dbg.2,
                control.idr_pending()
            ));
            enc_count = 0;
            enc_ms_sum = 0.0;
            enc_ms_max = 0.0;
            stage_count = 0;
            stage_ms_sum = 0.0;
            stage_ms_max = 0.0;
            dxgi_copy_ms_sum = 0.0;
            dxgi_copy_ms_max = 0.0;
            readback_ms_sum = 0.0;
            readback_ms_max = 0.0;
            conversion_ms_sum = 0.0;
            conversion_ms_max = 0.0;
            mirror_ms_sum = 0.0;
            mirror_ms_max = 0.0;
            fresh_encode_count = 0;
            fresh_encode_ms_sum = 0.0;
            fresh_encode_ms_max = 0.0;
            restaged_encode_count = 0;
            restaged_encode_ms_sum = 0.0;
            restaged_encode_ms_max = 0.0;
            blank_encode_count = 0;
            blank_encode_ms_sum = 0.0;
            blank_encode_ms_max = 0.0;
            restage_ms_sum = 0.0;
            restage_ms_max = 0.0;
            restage_copied = 0;
            restage_skipped = 0;
            restage_unavailable = 0;
            bytes_sum = 0;
            encode_submitted = 0;
            encode_skipped_no_new = 0;
            dbg = (0, 0, 0);
            sec = Instant::now();
        }
    }
    log("NVENC control closed; dropping encoder before exit");
    0
}

/// Best-effort GPU name for `capenc probe-matrix`'s `environments[].host.gpu`
/// field: the first NVIDIA adapter's description if one exists, else the
/// first adapter of any vendor. Enumeration only — no device is created —
/// so this needs no `nvenc`/`mf` feature and is available even in the
/// default build, unlike an actual encoder trial.
unsafe fn first_adapter_description() -> Option<String> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
    let mut first_any: Option<String> = None;
    let mut index = 0u32;
    loop {
        let adapter: IDXGIAdapter = match factory.EnumAdapters(index) {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        index += 1;
        let Ok(description) = adapter.GetDesc() else {
            continue;
        };
        let name: String = description
            .Description
            .iter()
            .take_while(|&&character| character != 0)
            .map(|&character| char::from_u32(u32::from(character)).unwrap_or('?'))
            .collect();
        if description.VendorId == 0x10de {
            return Some(name);
        }
        first_any.get_or_insert(name);
    }
    first_any
}

/// NVENC encode hot-path self-test. Feeds NVENC a synthetic BGRA texture that
/// moves each frame (via a CPU staging texture -> CopyResource into the
/// registered input, mirroring the real DXGI->input copy), so we can validate
/// the encode + measure `stage+encode` ms and bitrate on real hardware WITHOUT
/// a live desktop (which a headless VM can't reliably present). This is the
/// number that justifies the Rust path (target: stage+encode <= 6 ms at 4K).
/// Build a D3D11 device on the exact selected output's adapter without opening
/// Desktop Duplication. Admission encoding therefore cannot silently borrow
/// another NVIDIA adapter.
#[cfg(feature = "nvenc")]
unsafe fn create_encode_device(
    selector: &OutputSelector<'_>,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let found = Capture::find_output_device(selector)?;
    if found.vendor_id != 0x10de {
        return Err(windows::core::Error::new(
            E_ACCESSDENIED,
            format!(
                "selected adapter {:?} is not NVIDIA and cannot host NVENC",
                found.adapter_name
            ),
        ));
    }
    log(&format!(
        "synthetic encode device bound to {} -> adapter {} ({:?})",
        selector.describe(),
        found.adapter_index,
        found.adapter_name
    ));
    Ok((found.device, found.context))
}

/// Preserve the standalone headless self-test contract: it diagnoses the first
/// usable NVIDIA adapter without requiring an attached desktop output.
#[cfg(feature = "nvenc")]
unsafe fn create_headless_selftest_device(
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
    let mut adapter_index = 0u32;
    let mut last_error = None;
    loop {
        let adapter: IDXGIAdapter = match factory.EnumAdapters(adapter_index) {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        adapter_index += 1;
        let description = match adapter.GetDesc() {
            Ok(description) => description,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        if description.VendorId != 0x10de {
            continue;
        }

        let mut device = None;
        let mut context = None;
        match D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        ) {
            Ok(()) => {
                if let (Some(device), Some(context)) = (device, context) {
                    let name: String = description
                        .Description
                        .iter()
                        .take_while(|&&character| character != 0)
                        .map(|&character| char::from_u32(character as u32).unwrap_or('?'))
                        .collect();
                    log(&format!(
                        "selftest device on NVIDIA adapter {} ({name})",
                        adapter_index - 1
                    ));
                    return Ok((device, context));
                }
                last_error = Some(windows::core::Error::new(
                    E_ACCESSDENIED,
                    "D3D11 returned no selftest device or context",
                ));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        windows::core::Error::new(E_ACCESSDENIED, "no NVIDIA adapter found for NVENC selftest")
    }))
}

#[cfg(feature = "nvenc")]
unsafe fn run_selftest(
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    codec: &str,
    w: u32,
    h: u32,
    color: crate::ColorSpec,
    qp_map_policy: crate::qp_map::QpMapPolicy,
    framed: bool,
) -> i32 {
    use std::time::Duration;

    let mut encoder = match crate::nvenc::Encoder::new(
        &device,
        &context,
        w,
        h,
        codec,
        color,
        // The selftest answers "does this format initialise and encode?", not
        // "how good does it look", so it uses the shipped default rather than
        // whatever a session happened to request.
        arcen_media::EncodeIntent::default(),
        qp_map_policy,
        false,
    ) {
        Ok(e) => e,
        Err(e) => {
            log(&format!("NVENC init failed: {e}"));
            return 4;
        }
    };
    log(&format!(
        "NVENC selftest: {w}x{h} codec={codec} chroma={:?} depth={:?} range={:?} matrix={:?} \
         (synthetic content)",
        color.chroma, color.bit_depth, color.range, color.matrix
    ));

    // CPU-writable staging texture; mutate it each frame then CopyResource into
    // the encoder's registered input (same GPU copy the live path does).
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    if let Err(e) = device.CreateTexture2D(&desc, None, Some(&mut staging)) {
        log(&format!("CreateTexture2D(staging): {e:?}"));
        return 6;
    }
    let staging = staging.expect("staging texture");
    let staging_res: ID3D11Resource = staging.cast().expect("staging as resource");

    let mut stdout = std::io::stdout();
    let target_dt = Duration::from_micros(16_666); // cap at 60 fps
    let mut next = Instant::now();
    let mut frame: u32 = 0;
    let mut sec = Instant::now();
    let (mut cnt, mut ms_sum, mut ms_max, mut bytes) = (0u64, 0.0f64, 0.0f64, 0u64);

    loop {
        // Paint synthetic moving content into the staging texture.
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if context
            .Map(&staging_res, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
            .is_ok()
        {
            // Realistic desktop model: a STATIC gradient background (encodes to
            // near-zero residual after the first frame) plus one moving block —
            // like a cursor/window dragging over a mostly-static UI. Set
            // CAPENC_FULLCHURN=1 to force worst-case full-frame churn instead.
            let full_churn = std::env::var_os("CAPENC_FULLCHURN").is_some();
            let base = mapped.pData as *mut u8;
            let pitch = mapped.RowPitch as usize;
            let (bw, bh) = (320usize, 320usize);
            let bx = ((frame * 7) as usize) % (w as usize - bw);
            let by = ((frame * 3) as usize) % (h as usize - bh);
            for y in 0..h as usize {
                let row = base.add(y * pitch) as *mut u32;
                let rs = std::slice::from_raw_parts_mut(row, w as usize);
                let g = if full_churn {
                    ((y as u32 + frame) & 0xFF) << 8
                } else {
                    ((y as u32) & 0xFF) << 8
                };
                let in_block_y = y >= by && y < by + bh;
                for (x, pixel) in rs.iter_mut().enumerate() {
                    if in_block_y && x >= bx && x < bx + bw {
                        *pixel = 0xFFFF_FFFF; // bright moving block
                        continue;
                    }
                    let b = if full_churn {
                        (x as u32 + frame) & 0xFF
                    } else {
                        (x as u32) & 0xFF
                    };
                    let r = ((x as u32 ^ y as u32) & 0xFF) << 16;
                    *pixel = 0xFF00_0000 | r | g | b;
                }
            }
            context.Unmap(&staging_res, 0);
        }

        // Hot-path under test: GPU copy into the registered input + NVENC encode.
        let t0 = Instant::now();
        if let Err(e) = encoder.stage(&staging) {
            log(&format!("stage: {e}"));
            return 7;
        }
        match encoder.encode(frame == 0) {
            Ok(out) => {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                if let Some(au) = out {
                    if crate::write_access_unit(&mut stdout, &au, framed).is_err() {
                        return 0;
                    }
                    cnt += 1;
                    ms_sum += ms;
                    if ms > ms_max {
                        ms_max = ms;
                    }
                    bytes += au.len() as u64;
                }
            }
            Err(e) => {
                log(&format!("encode: {e}"));
                return 5;
            }
        }
        frame = frame.wrapping_add(1);

        if sec.elapsed().as_secs_f64() >= 1.0 {
            let avg = if cnt > 0 { ms_sum / cnt as f64 } else { 0.0 };
            log(&format!(
                "SELFTEST enc/s={cnt} stage+encode avg_ms={avg:.2} max_ms={ms_max:.2} mbps={:.1}",
                (bytes * 8) as f64 / 1_000_000.0
            ));
            cnt = 0;
            ms_sum = 0.0;
            ms_max = 0.0;
            bytes = 0;
            sec = Instant::now();
        }

        // Pace to <= 60 fps so enc/s reflects a sustainable rate.
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += target_dt;
        if next < now {
            next = now + target_dt;
        }
    }
}

#[cfg(feature = "nvenc")]
unsafe fn run_admission_probe(
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    codec: &str,
    color: crate::ColorSpec,
    qp_map_policy: crate::qp_map::QpMapPolicy,
    options: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    let mut encoder = match crate::nvenc::Encoder::new(
        &device,
        &context,
        options.width,
        options.height,
        codec,
        color,
        // An admission probe asks whether the format initialises at all.
        arcen_media::EncodeIntent::default(),
        qp_map_policy,
        false,
    ) {
        Ok(encoder) => encoder,
        Err(error) => {
            log(&format!("admission probe NVENC init failed: {error}"));
            return 4;
        }
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: options.width,
        Height: options.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    if let Err(error) = device.CreateTexture2D(&desc, None, Some(&mut staging)) {
        log(&format!(
            "admission probe CreateTexture2D(staging): {error:?}"
        ));
        return 6;
    }
    let staging = staging.expect("admission staging texture");
    let staging_resource: ID3D11Resource = staging.cast().expect("admission staging resource");
    let mut frame = 0u32;
    let result =
        crate::admission_probe::run_probe_loop(options, std::io::stdout().lock(), |input| {
            frame = frame.wrapping_add(1);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging_resource, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .map_err(|error| format!("admission probe map: {error:?}"))?;
            let changed_rows = if input.kind == arcen_media::RepresentativeFrameKind::FullMotion {
                options.height as usize
            } else {
                (options.height as usize)
                    .saturating_mul(usize::from(input.dirty_ratio.basis_points()))
                    .div_ceil(10_000)
                    .max(1)
            };
            let pitch = mapped.RowPitch as usize;
            let width = options.width as usize;
            let base = mapped.pData.cast::<u8>();
            for y in 0..changed_rows {
                let row = std::slice::from_raw_parts_mut(base.add(y * pitch).cast::<u32>(), width);
                for (x, pixel) in row.iter_mut().enumerate() {
                    *pixel = 0xff00_0000
                        | ((frame.wrapping_add(x as u32) & 0xff) << 16)
                        | ((frame.wrapping_add(y as u32) & 0xff) << 8)
                        | (frame & 0xff);
                }
            }
            context.Unmap(&staging_resource, 0);
            let started = Instant::now();
            encoder
                .stage(&staging)
                .map_err(|error| format!("admission probe stage: {error}"))?;
            let output = encoder
                .encode(input.force_idr)
                .map_err(|error| format!("admission probe encode: {error}"))?;
            Ok(crate::admission_probe::ProbeEncodeResult {
                encode_latency: started.elapsed(),
                delivered: output.is_some(),
            })
        });
    match result {
        Ok(()) => 0,
        Err(error) => {
            log(&format!("admission probe failed: {error}"));
            5
        }
    }
}

/// Stage 1: capture-only measurement loop (no NVENC feature). Proves the DXGI
/// path delivers live textures; reports new/timeout/cursor-only + fps.
#[cfg(not(feature = "nvenc"))]
fn run_capture_only(mut cap: Capture) -> ! {
    let mut frames = 0u64;
    let mut last_fmt = None;
    let mut last = Instant::now();
    let mut dbg = (0u64, 0u64, 0u64); // new, timeout, cursor-only
    loop {
        match unsafe { cap.acquire(1000, &mut dbg) } {
            Ok(Some(tex)) => {
                let mut desc: D3D11_TEXTURE2D_DESC = Default::default();
                unsafe { tex.GetDesc(&mut desc) };
                frames += 1;
                last_fmt = Some((desc.Format, desc.Width, desc.Height));
            }
            Ok(None) => {}
            Err(e) => {
                log(&format!("acquire error: {e:?}"));
                std::process::exit(3);
            }
        }
        if last.elapsed().as_secs_f64() >= 1.0 {
            log(&format!(
                "new={} timeout={} cursor_only={} total_frames={} last_fmt={:?}",
                dbg.0, dbg.1, dbg.2, frames, last_fmt
            ));
            dbg = (0, 0, 0);
            last = Instant::now();
        }
    }
}

/// Enumerate every DXGI adapter + output, report AttachedToDesktop /
/// DesktopCoordinates, and for each desktop-attached output build a device on
/// its OWN adapter, duplicate, and poll AcquireNextFrame for ~2s to see whether
/// that specific head actually DELIVERS frames. Diagnostic for headless / vGPU
/// hosts where DuplicateOutput succeeds but AcquireNextFrame times out forever.
unsafe fn enum_and_probe() {
    let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
        Ok(f) => f,
        Err(e) => {
            log(&format!("enumouts: CreateDXGIFactory1 failed: {e:?}"));
            return;
        }
    };
    let mut ai = 0u32;
    loop {
        let adapter: IDXGIAdapter = match factory.EnumAdapters(ai) {
            Ok(a) => a,
            Err(_) => break,
        };
        let adesc = adapter.GetDesc().unwrap_or_default();
        let aname = String::from_utf16_lossy(&adesc.Description)
            .trim_end_matches('\u{0}')
            .to_string();
        log(&format!(
            "adapter {ai}: '{aname}' vendor=0x{:04X} device=0x{:04X}",
            adesc.VendorId, adesc.DeviceId
        ));
        let mut oi = 0u32;
        loop {
            let output = match adapter.EnumOutputs(oi) {
                Ok(o) => o,
                Err(_) => break,
            };
            let odesc = match output.GetDesc() {
                Ok(d) => d,
                Err(e) => {
                    log(&format!("  output {oi}: GetDesc failed: {e:?}"));
                    oi += 1;
                    continue;
                }
            };
            let r = odesc.DesktopCoordinates;
            let attached = odesc.AttachedToDesktop.as_bool();
            log(&format!(
                "  output {oi}: attached={attached} rect=({},{})-({},{}) rot={}",
                r.left, r.top, r.right, r.bottom, odesc.Rotation.0
            ));
            oi += 1;
            if !attached {
                continue;
            }
            let output1: IDXGIOutput1 = match output.cast() {
                Ok(o) => o,
                Err(e) => {
                    log(&format!("    cast IDXGIOutput1 failed: {e:?}"));
                    continue;
                }
            };
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            if let Err(e) = D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            ) {
                log(&format!("    D3D11CreateDevice failed: {e:?}"));
                continue;
            }
            let device = device.unwrap();
            let dupl = match output1.DuplicateOutput(&device) {
                Ok(d) => d,
                Err(e) => {
                    log(&format!("    DuplicateOutput failed: {e:?}"));
                    continue;
                }
            };
            let (mut new_n, mut to_n, mut cur_n) = (0u64, 0u64, 0u64);
            let start = Instant::now();
            while start.elapsed().as_secs_f64() < 2.0 {
                let mut info: DXGI_OUTDUPL_FRAME_INFO = Default::default();
                let mut resource: Option<IDXGIResource> = None;
                match dupl.AcquireNextFrame(200, &mut info, &mut resource) {
                    Ok(()) => {
                        if info.AccumulatedFrames > 0 || info.LastPresentTime != 0 {
                            new_n += 1;
                        } else {
                            cur_n += 1;
                        }
                        let _ = dupl.ReleaseFrame();
                    }
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => to_n += 1,
                    Err(e) => {
                        log(&format!("    AcquireNextFrame error: {e:?}"));
                        break;
                    }
                }
            }
            log(&format!(
                "    PROBE 2s: new={new_n} cursor_only={cur_n} timeout={to_n} -> {}",
                if new_n > 0 {
                    "DELIVERS FRAMES"
                } else {
                    "NO FRAMES"
                }
            ));
        }
        ai += 1;
    }
    log("enumouts: done");
}

/// Fixed geometry for every `capenc probe-matrix` trial. The matrix probes
/// *whether a combination initialises*, not performance at a particular
/// resolution, so a modest, fast-to-encode size keeps an 11-row matrix (two
/// backends each) a few seconds of wall time rather than minutes.
const PROBE_WIDTH: u32 = 1920;
const PROBE_HEIGHT: u32 = 1080;

/// The `ColorSpec` one probe-matrix row resolves to, via the *exact* same
/// `crate::requested_color` function every real run entry point now calls —
/// not `ColorSpec::from_variant(row)` directly. Bypassing `requested_color`
/// here would mean the probe could report a row as `ok` when the real
/// argv-parsing path it is meant to stand in for was never actually
/// exercised; routing through it here means a bug in that shared resolution
/// function shows up as a probe-matrix failure too, not only in a live run.
/// `row.id()` always round-trips through `VideoVariant::from_id` (enforced
/// by `arcen_media::video::variant`'s own tests), so this cannot fail.
fn variant_color(row: arcen_media::video::VideoVariant) -> crate::ColorSpec {
    crate::requested_color(&[format!("variant={}", row.id())], false)
        .expect("a PROBE_MATRIX row's own id always round-trips")
}

/// `capenc probe-matrix [--output <path>]`: walks every row of
/// `arcen_media::video::PROBE_MATRIX`, attempts a real encoder
/// initialisation (NVENC, plus Media Foundation where compiled in) for each,
/// and prints (or writes) the JSON report `crate::probe_matrix` renders. See
/// that module's doc for the full output contract and rationale — this
/// function only supplies the real, GPU-touching per-backend trials, kept
/// out of `probe_matrix.rs` so that module stays unit-testable without a
/// device.
///
/// Never fails the whole run because one row could not initialise — a
/// failing row is itself the finding — so this returns `0` once every row
/// has been attempted; only an unwritable `--output` path exits non-zero.
fn run_probe_matrix_subcommand(args: &[String]) -> i32 {
    // `--roundtrip-pattern`/`--roundtrip-output-dir`: see `probe_matrix.rs`'s
    // module doc. Parsed up front so a typo'd pattern token or a
    // half-given pair of flags is reported and exits non-zero rather than
    // silently skipping the round-trip sources the caller asked for.
    let roundtrip = match crate::probe_matrix::parse_roundtrip_request(args) {
        Ok(roundtrip) => roundtrip,
        Err(error) => {
            log(&format!("probe-matrix: {error}"));
            return 2;
        }
    };

    let host = crate::probe_matrix::HostInfo {
        os: std::env::consts::OS.to_string(),
        gpu: unsafe { first_adapter_description() }.unwrap_or_default(),
        driver_version: String::new(),
        nvenc_generation: String::new(),
    };
    let environment = crate::probe_matrix::EnvironmentInfo::new(host);

    // The trial device (or, without the `nvenc` feature, a unit placeholder
    // `nvenc_attempt_for_row`'s other definition ignores) is created once and
    // reused across every row: only the NVENC *session* — `Encoder::new`,
    // called fresh per row below — needs to be independent per trial.
    #[cfg(feature = "nvenc")]
    let nvenc_device = unsafe { create_headless_selftest_device() };
    #[cfg(feature = "nvenc")]
    if let Err(error) = &nvenc_device {
        log(&format!(
            "probe-matrix: no NVIDIA adapter available for NVENC trials: {error:?}"
        ));
    }
    #[cfg(not(feature = "nvenc"))]
    let nvenc_device = ();

    if let Some(request) = &roundtrip {
        log(&format!(
            "probe-matrix: also writing round-trip sources for pattern `{}` to {} (NVENC only \
             -- see write_roundtrip_bitstream_for_row's doc)",
            request.pattern.token(),
            request.output_dir.display(),
        ));
    }

    let report = crate::probe_matrix::build_report(environment, |row| {
        let nvenc_outcome = nvenc_attempt_for_row(&nvenc_device, row);
        if let Some(request) = &roundtrip {
            if let Err(error) = write_roundtrip_bitstream_for_row(&nvenc_device, row, request) {
                log(&format!(
                    "probe-matrix: round-trip source for `{}` was not written: {error}",
                    row.id()
                ));
            }
        }
        vec![("NVENC", nvenc_outcome), ("MF", mf_attempt_for_row(row))]
    });

    let json = report.render();
    match crate::probe_matrix::output_path_from_args(args) {
        Some(path) => match std::fs::write(&path, &json) {
            Ok(()) => {
                println!("probe-matrix: wrote {}", path.display());
                0
            }
            Err(error) => {
                log(&format!(
                    "probe-matrix: failed to write {}: {error}",
                    path.display()
                ));
                2
            }
        },
        None => {
            println!("{json}");
            0
        }
    }
}

/// One backend's real trial for one row, with the `nvenc` feature compiled
/// in: a fresh `Encoder::new` per row (see the module doc on `PixelFormat`
/// for why this is never a reconfigure of a shared session), classified via
/// `NvencInitError::unavailable_reason` into the three outcomes
/// `crate::probe_matrix` understands, then (on success) a short measurement
/// burst via `run_probe_burst`.
#[cfg(feature = "nvenc")]
fn nvenc_attempt_for_row(
    device: &windows::core::Result<(ID3D11Device, ID3D11DeviceContext)>,
    row: arcen_media::video::VideoVariant,
) -> crate::probe_matrix::EncoderAttemptOutcome {
    use crate::probe_matrix::EncoderAttemptOutcome;

    let (device, context) = match device {
        Ok(pair) => pair,
        Err(error) => {
            return EncoderAttemptOutcome::Failed {
                detail: format!("no NVIDIA adapter available for a trial device: {error:?}"),
            };
        }
    };
    let codec_token = row.video.codec.token();
    let color = variant_color(row);
    let mut encoder = match unsafe {
        crate::nvenc::Encoder::new(
            device,
            context,
            PROBE_WIDTH,
            PROBE_HEIGHT,
            codec_token,
            color,
            // The matrix probes which colour formats encode, not how well.
            arcen_media::EncodeIntent::default(),
            crate::qp_map::QpMapPolicy::Off,
            false,
        )
    } {
        Ok(encoder) => encoder,
        Err(error) => {
            let detail = error.to_string();
            return match error.unavailable_reason() {
                Some(arcen_media::video::BackendUnavailableReason::UnsupportedConfiguration) => {
                    EncoderAttemptOutcome::Unsupported { detail }
                }
                _ => EncoderAttemptOutcome::Failed { detail },
            };
        }
    };
    match unsafe { run_probe_burst(device, context, &mut encoder) } {
        Ok(burst) => EncoderAttemptOutcome::Ok {
            sustained_fps: Some(burst.fps),
            bitrate_mbps: Some(burst.bitrate_mbps),
            note: burst.note,
        },
        // `NvEncInitializeEncoder` itself already succeeded by this point —
        // that is the finding this subcommand exists to make, so a burst
        // failure downgrades only the measurement, never the verdict.
        Err(error) => EncoderAttemptOutcome::Ok {
            sustained_fps: None,
            bitrate_mbps: None,
            note: format!("init succeeded but the measurement burst failed: {error}"),
        },
    }
}

/// Without the `nvenc` feature there is no encoder to trial at all.
#[cfg(not(feature = "nvenc"))]
fn nvenc_attempt_for_row(
    _device: &(),
    _row: arcen_media::video::VideoVariant,
) -> crate::probe_matrix::EncoderAttemptOutcome {
    crate::probe_matrix::EncoderAttemptOutcome::NotCompiled {
        detail: "capenc was built without --features nvenc".to_string(),
    }
}

/// One post-init measurement burst's results: real, measured (not
/// requested) fps and bitrate over a short synthetic run, plus whatever else
/// is worth keeping in `notes` (see `run_probe_burst`).
#[cfg(feature = "nvenc")]
struct ProbeBurst {
    fps: f64,
    bitrate_mbps: f64,
    note: String,
}

/// Frames submitted per row's measurement burst: comfortably more than
/// `nvenc.rs`'s two-deep pipeline (`DEPTH`), so "no access unit produced" is
/// a real finding about the row rather than an artefact of a burst too short
/// to drain the pipeline.
#[cfg(feature = "nvenc")]
const PROBE_BURST_FRAMES: u32 = 32;

/// Feeds `encoder` a `PROBE_BURST_FRAMES`-frame synthetic burst as fast as
/// possible (not real-time-paced — see the returned note) and measures the
/// resulting fps/bitrate, mirroring `run_selftest`'s own synthetic-content
/// approach (a CPU-writable staging texture, painted then handed to
/// `Encoder::stage`) but bounded instead of run forever.
#[cfg(feature = "nvenc")]
unsafe fn run_probe_burst(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    encoder: &mut crate::nvenc::Encoder,
) -> Result<ProbeBurst, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: PROBE_WIDTH,
        Height: PROBE_HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    device
        .CreateTexture2D(&desc, None, Some(&mut staging))
        .map_err(|error| format!("probe burst CreateTexture2D(staging): {error:?}"))?;
    let staging = staging.ok_or_else(|| "probe burst staging texture null".to_string())?;
    let staging_res: ID3D11Resource = staging
        .cast()
        .map_err(|error| format!("probe burst staging as resource: {error:?}"))?;

    let mut produced_frames = 0u32;
    let mut produced_bytes = 0u64;
    let mut first_au_frame = None;
    let start = Instant::now();
    for frame in 0..PROBE_BURST_FRAMES {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if context
            .Map(&staging_res, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
            .is_ok()
        {
            // Cheap synthetic content: a moving gradient. Enough for a real
            // encode + a non-trivial (non all-zero) access unit; the actual
            // pixels are irrelevant to the finding this burst measures.
            let base = mapped.pData.cast::<u8>();
            let pitch = mapped.RowPitch as usize;
            for y in 0..PROBE_HEIGHT as usize {
                let row_ptr = base.add(y * pitch).cast::<u32>();
                let row_pixels = std::slice::from_raw_parts_mut(row_ptr, PROBE_WIDTH as usize);
                for (x, pixel) in row_pixels.iter_mut().enumerate() {
                    let moves = ((x as u32 + frame * 4) & 0xFF) << 8;
                    let vertical = (y as u32 & 0xFF) << 16;
                    *pixel = 0xFF00_0000 | vertical | moves | ((x as u32 ^ y as u32) & 0xFF);
                }
            }
            context.Unmap(&staging_res, 0);
        }

        encoder
            .stage(&staging)
            .map_err(|error| format!("probe burst stage: {error}"))?;
        if let Some(access_unit) = encoder
            .encode(frame == 0)
            .map_err(|error| format!("probe burst encode: {error}"))?
        {
            produced_frames += 1;
            produced_bytes += access_unit.len() as u64;
            first_au_frame.get_or_insert(frame);
        }
    }
    let elapsed = start.elapsed().as_secs_f64().max(f64::EPSILON);
    let fps = f64::from(produced_frames) / elapsed;
    let bitrate_mbps = (produced_bytes as f64 * 8.0 / 1_000_000.0) / elapsed;
    let note = match first_au_frame {
        Some(index) => format!(
            "first access unit at burst frame {index}; {produced_frames}/{PROBE_BURST_FRAMES} \
             frames produced an access unit over a {PROBE_BURST_FRAMES}-frame back-to-back burst \
             at {PROBE_WIDTH}x{PROBE_HEIGHT} (measured, not real-time-paced -- not a sustained \
             playback figure)"
        ),
        None => format!(
            "encoder accepted every frame but produced no access unit within a \
             {PROBE_BURST_FRAMES}-frame burst"
        ),
    };
    Ok(ProbeBurst {
        fps,
        bitrate_mbps,
        note,
    })
}

/// Writes one row's real round-trip colour source: encodes `request.pattern`
/// through a fresh NVENC session for `row`'s format and writes the resulting
/// bitstream (plus the shared metadata file) via
/// `crate::probe_matrix::write_roundtrip_outputs`.
///
/// NVENC only, deliberately: this is the encoder the product's colour work
/// targets (`hevc-444-10-full-bt709`, the row this whole harness exists to
/// measure, has no Media Foundation path at all), and folding MF in too
/// would double the surface here for a codec (H.264) the round-trip
/// harness does not need to prove. Rows this harness doesn't cover (H.264,
/// and AV1 -- which now has a real NVENC encode path, see `nvenc.rs`, but
/// is not the row this harness exists to prove) are refused with the same
/// message shape `probe_one_row` uses for a codec with no encoder at all.
///
/// # Errors
///
/// Returns a description of whatever step failed (no adapter, encoder
/// init, encode, or the final file write) rather than panicking -- the
/// caller logs this and continues with the next row, exactly like every
/// other per-row trial in this module.
#[cfg(feature = "nvenc")]
fn write_roundtrip_bitstream_for_row(
    device: &windows::core::Result<(ID3D11Device, ID3D11DeviceContext)>,
    row: arcen_media::video::VideoVariant,
    request: &crate::probe_matrix::RoundtripRequest,
) -> Result<(), String> {
    if !matches!(
        row.video.codec,
        arcen_media::VideoCodec::H264 | arcen_media::VideoCodec::H265
    ) {
        return Err(format!(
            "{:?} is not a row this round-trip harness proves (scoped to the HEVC 4:4:4 \
             10-bit flagship row); see write_roundtrip_bitstream_for_row's doc",
            row.video.codec
        ));
    }
    let (device, context) = device
        .as_ref()
        .map_err(|error| format!("no NVIDIA adapter available for a trial device: {error:?}"))?;
    let codec_token = row.video.codec.token();
    let color = variant_color(row);
    let mut encoder = unsafe {
        crate::nvenc::Encoder::new(
            device,
            context,
            PROBE_WIDTH,
            PROBE_HEIGHT,
            codec_token,
            color,
            // The matrix probes which colour formats encode, not how well.
            arcen_media::EncodeIntent::default(),
            crate::qp_map::QpMapPolicy::Off,
            false,
        )
    }
    .map_err(|error| format!("Encoder::new: {error}"))?;
    let bitstream = unsafe {
        encode_pattern_frame(
            device,
            context,
            &mut encoder,
            request.pattern,
            PROBE_WIDTH,
            PROBE_HEIGHT,
        )
    }?;
    crate::probe_matrix::write_roundtrip_outputs(
        request,
        PROBE_WIDTH,
        PROBE_HEIGHT,
        row,
        &bitstream,
    )
    .map_err(|error| format!("writing round-trip outputs: {error}"))
}

/// Without the `nvenc` feature there is no encoder to produce a round-trip
/// source with at all.
#[cfg(not(feature = "nvenc"))]
fn write_roundtrip_bitstream_for_row(
    _device: &(),
    _row: arcen_media::video::VideoVariant,
    _request: &crate::probe_matrix::RoundtripRequest,
) -> Result<(), String> {
    Err(
        "capenc was built without --features nvenc, so no real encoder is available to \
         produce a round-trip source"
            .to_string(),
    )
}

/// Encodes one deterministic [`arcen_media::test_pattern::TestPattern`]
/// frame through `encoder` as a real trial and returns the first coded
/// access unit produced.
///
/// `TestPattern` is a pure function of `(column, row, width, height)` (see
/// that module's doc), so only this access unit -- never the reference
/// pixels themselves -- needs to reach the Deck side: it regenerates the
/// identical reference locally from the pattern token and geometry recorded
/// in `roundtrip-meta.json`.
///
/// Every submission forces an IDR. Content is identical frame to frame, so
/// there is no meaningful inter-frame prediction to exercise here, and
/// forcing every frame keeps whichever access unit comes back self-contained
/// (parameter sets plus one complete intra picture) regardless of which
/// submission the encoder's internal pipeline latency happens to flush it
/// on -- mirroring [`run_probe_burst`]'s staging-texture technique, but
/// painting the chosen pattern instead of a synthetic gradient, and
/// stopping as soon as one access unit is produced rather than measuring a
/// whole burst.
///
/// # Errors
///
/// Returns a description of whatever D3D11/NVENC call failed, or (if no
/// access unit was produced within [`ROUNDTRIP_MAX_ATTEMPTS`] submissions) a
/// description of that too.
#[cfg(feature = "nvenc")]
unsafe fn encode_pattern_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    encoder: &mut crate::nvenc::Encoder,
    pattern: arcen_media::test_pattern::TestPattern,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    device
        .CreateTexture2D(&desc, None, Some(&mut staging))
        .map_err(|error| format!("round-trip CreateTexture2D(staging): {error:?}"))?;
    let staging = staging.ok_or_else(|| "round-trip staging texture null".to_string())?;
    let staging_res: ID3D11Resource = staging
        .cast()
        .map_err(|error| format!("round-trip staging as resource: {error:?}"))?;

    // Rendered once: `TestPattern` is a pure function, so the same BGRA
    // bytes are re-uploaded verbatim on every attempt below, matching the
    // BGRA byte order `DXGI_FORMAT_B8G8R8A8_UNORM` expects exactly (`[b, g,
    // r, a]` per pixel), so no conversion is needed before upload.
    let pixels = pattern.render_bgra(width as usize, height as usize);
    let row_bytes = width as usize * 4;

    for _attempt in 0..ROUNDTRIP_MAX_ATTEMPTS {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging_res, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
            .map_err(|error| format!("round-trip Map: {error:?}"))?;
        let base = mapped.pData.cast::<u8>();
        let pitch = mapped.RowPitch as usize;
        for y in 0..height as usize {
            let row_ptr = base.add(y * pitch);
            let src = &pixels[y * row_bytes..(y + 1) * row_bytes];
            std::ptr::copy_nonoverlapping(src.as_ptr(), row_ptr, src.len());
        }
        context.Unmap(&staging_res, 0);

        encoder
            .stage(&staging)
            .map_err(|error| format!("round-trip stage: {error}"))?;
        // Force an IDR on every submission -- see this function's doc.
        if let Some(access_unit) = encoder
            .encode(true)
            .map_err(|error| format!("round-trip encode: {error}"))?
        {
            return Ok(access_unit);
        }
    }
    Err(format!(
        "encoder accepted every frame but produced no access unit within \
         {ROUNDTRIP_MAX_ATTEMPTS} attempts"
    ))
}

/// Comfortably more than `nvenc.rs`'s two-deep pipeline (`DEPTH`), same
/// reasoning as [`PROBE_BURST_FRAMES`]: "no access unit produced" should be
/// a real finding about a row, not an artefact of giving up too early.
#[cfg(feature = "nvenc")]
const ROUNDTRIP_MAX_ATTEMPTS: u32 = 8;

/// The Media Foundation SW H.264 MFT's real trial for one row, with the `mf`
/// feature compiled in. The MFT is self-contained (no D3D11 device needed —
/// see `mf_encoder.rs`'s module doc), so this needs no headless device at
/// all; `mf_encoder::validate_mf_color` (called first, inside
/// `Encoder::new`) reports every colour rejection as `E_INVALIDARG`, which is
/// how this tells "MF cannot do this format" apart from a genuine COM/MFT
/// failure.
#[cfg(feature = "mf")]
fn mf_attempt_for_row(
    row: arcen_media::video::VideoVariant,
) -> crate::probe_matrix::EncoderAttemptOutcome {
    use crate::probe_matrix::EncoderAttemptOutcome;

    if row.video.codec != arcen_media::VideoCodec::H264 {
        return EncoderAttemptOutcome::Unsupported {
            detail: "the Media Foundation SW H.264 MFT in this build only ever encodes H.264"
                .to_string(),
        };
    }
    let color = variant_color(row);
    let cfg = crate::mf_encoder::Config {
        width: PROBE_WIDTH,
        height: PROBE_HEIGHT,
        fps: 30,
        bitrate_kbps: 5000,
        gop_frames: 60,
        profile: crate::mf_encoder::H264Profile::Main,
        color,
    };
    match crate::mf_encoder::Encoder::new(&cfg) {
        Ok(_encoder) => EncoderAttemptOutcome::Ok {
            sustained_fps: None,
            bitrate_mbps: None,
            note: "MF SW H.264 MFT initialised (no burst measured for this backend -- NVENC's \
                   burst already measures fps/bitrate for this row)"
                .to_string(),
        },
        Err(error) => {
            let detail = format!("{error:?}");
            if error.code() == E_INVALIDARG {
                EncoderAttemptOutcome::Unsupported { detail }
            } else {
                EncoderAttemptOutcome::Failed { detail }
            }
        }
    }
}

/// Without the `mf` feature there is no Media Foundation encoder to trial.
#[cfg(not(feature = "mf"))]
fn mf_attempt_for_row(
    _row: arcen_media::video::VideoVariant,
) -> crate::probe_matrix::EncoderAttemptOutcome {
    crate::probe_matrix::EncoderAttemptOutcome::NotCompiled {
        detail: "capenc was built without --features mf".to_string(),
    }
}

pub fn run() -> ! {
    run_with_args(std::env::args().collect())
}

/// Entry point for the multi-call host, which has already stripped its own
/// dispatcher token.
///
/// Taking the vector rather than re-reading the environment is load-bearing:
/// the arguments here are positional, so re-reading `std::env::args()` would
/// leave the `capenc` subcommand at index 1 and shift every positional
/// argument by one. The visible symptom is not an argument error but a codec
/// read as the output index.
pub fn run_with_args(args: Vec<String>) -> ! {
    if args.iter().any(|a| a == "color-probe") {
        // Research diagnostic: does this host deliver >8bpc from Desktop
        // Duplication at all? See docs/internal/ten-bit-source-capture.md.
        let enable_hdr = args.iter().any(|a| a == "--enable-hdr");
        let disable_hdr = args.iter().any(|a| a == "--disable-hdr");
        let wgc_only = args.iter().any(|a| a == "--wgc-only");
        unsafe { crate::win_color_probe::run_with(enable_hdr, disable_hdr, wgc_only) };
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "enumouts") {
        unsafe { enum_and_probe() };
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "probe-matrix") {
        std::process::exit(run_probe_matrix_subcommand(&args));
    }
    let output_index: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let cursor_mode = match crate::cursor_mode_from_args(&args) {
        Ok(mode) => mode,
        Err(error) => {
            log(error);
            std::process::exit(2);
        }
    };

    // Encoder selection may appear anywhere in argv. Product auto tries NVENC
    // first and then source-built OpenH264; `mf` remains a standalone
    // comparison feature only.
    let encoder_choice = args
        .iter()
        .find_map(|a| a.strip_prefix("encoder="))
        .unwrap_or("auto")
        .to_ascii_lowercase();
    // `adapter=<substring>`/`adapter-output=<index>`/`device=<name>` pick a
    // specific DXGI adapter/output. The pier threads these from a freshly
    // re-resolved stable `(adapter LUID, target)` binding on every backend —
    // DDA, the in-process WGC fallback, and the software paths — so a
    // display re-enumeration can never silently bind the wrong physical
    // output just because `output_index`'s positional ordinal shifted. See
    // `OutputSelector`.
    let adapter_hint = args
        .iter()
        .find_map(|a| a.strip_prefix("adapter="))
        .map(|s| s.to_string());
    let adapter_output_index = args
        .iter()
        .find_map(|a| a.strip_prefix("adapter-output="))
        .and_then(|s| s.parse().ok());
    let device_name = args
        .iter()
        .find_map(|a| a.strip_prefix("device="))
        .map(|s| s.to_string());
    let selector = OutputSelector {
        global_output_index: output_index,
        adapter_hint: adapter_hint.as_deref(),
        adapter_output_index,
        device_name: device_name.as_deref(),
    };
    // Logged unconditionally (every backend below re-derives its own
    // resolution from `selector`, but an `mf`-only build never reaches the
    // `nvenc`-gated call sites that would otherwise be `selector`'s only
    // reader) so the parent's pre-READY diagnostics always show exactly what
    // this child was asked to bind, even before backend-specific resolution
    // runs.
    log(&format!(
        "capenc: requested output selector: {}",
        selector.describe()
    ));
    // `bitrate=<kbps>` overrides the default software-encoder CBR bitrate.
    #[cfg(any(feature = "mf", feature = "software-h264"))]
    let bitrate_kbps: u32 = args
        .iter()
        .find_map(|a| a.strip_prefix("bitrate="))
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);

    // argv[2] = codec ("h264" | "h265").
    #[cfg(any(feature = "nvenc", feature = "mf", feature = "software-h264"))]
    let codec = args.get(2).cloned().unwrap_or_else(|| "h264".to_string());
    #[cfg(feature = "nvenc")]
    let fps_arg = args.get(3).filter(|value| value.as_str() != "selftest");
    #[cfg(feature = "nvenc")]
    let fps = fps_arg
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(60);
    // For software H.264, the same argv[3] fps position applies; default to 30
    // fps since SW H.264 encoding of a full desktop at 60 fps on a 2-vCPU VM
    // is not realistic.
    #[cfg(any(feature = "mf", feature = "software-h264"))]
    let software_fps: u32 = args
        .get(3)
        .filter(|value| value.as_str() != "selftest")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(30);
    #[cfg(any(feature = "nvenc", feature = "mf", feature = "software-h264"))]
    let yuv444 = args.iter().any(|value| value == "yuv444");
    // The single resolved colour contract for this run: an explicit
    // `variant=<id>` wins over the legacy `yuv444` token, and it is this one
    // value — never a separately re-derived `ColorSpec::legacy(...)` — that
    // must reach both the encoder init below and `resolved_media_plan`'s
    // READY line, or the two can silently disagree about what was actually
    // requested. See `crate::requested_color`.
    #[cfg(any(feature = "nvenc", feature = "mf", feature = "software-h264"))]
    let color = match crate::requested_color(&args, yuv444) {
        Ok(color) => color,
        Err(error) => {
            log(&format!("invalid variant: {error}"));
            std::process::exit(2);
        }
    };
    let intent = match crate::requested_intent(&args) {
        Ok(intent) => intent,
        Err(error) => {
            log(&format!("invalid intent: {error}"));
            std::process::exit(2);
        }
    };
    let qp_map_policy = match crate::requested_qp_map(&args) {
        Ok(policy) => policy,
        Err(error) => {
            log(&format!("invalid qp-map: {error}"));
            std::process::exit(2);
        }
    };
    let framed = crate::framed_output_from_args(&args);
    let admission_probe = match crate::admission_probe::options_from_args(&args) {
        Ok(options) => options,
        Err(error) => {
            log(&format!("invalid admission probe: {error}"));
            std::process::exit(2);
        }
    };
    #[cfg(feature = "nvenc")]
    let selftest_index = args.iter().position(|value| value == "selftest");
    #[cfg(feature = "nvenc")]
    let selftest = selftest_index.is_some();
    #[cfg(feature = "nvenc")]
    let (st_w, st_h) = selftest_index
        .and_then(|index| args.get(index + 1))
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((3840u32, 2160u32));

    log(&format!(
        "capenc: encoder-select={} (compiled: nvenc={} mf={} software-h264={})",
        encoder_choice,
        cfg!(feature = "nvenc"),
        cfg!(feature = "mf"),
        cfg!(feature = "software-h264"),
    ));
    // Echo the argument vector as received. Positional arguments plus a
    // multi-call dispatcher that strips a subcommand make an off-by-one both
    // easy to introduce and invisible in every other log line.
    log(&format!("capenc: argv={args:?}"));

    // Portable OpenH264 is both the explicit software backend and the product
    // auto fallback.
    #[cfg(feature = "software-h264")]
    if encoder_choice == "software-h264" {
        if !codec.eq_ignore_ascii_case("h264")
            || color.chroma != arcen_media::ChromaSubsampling::Yuv420
            || color.bit_depth != arcen_media::BitDepth::Eight
        {
            log("OpenH264 software encoding requires h264 + yuv420 8-bit");
            std::process::exit(2);
        }
        let options = crate::win_mf::OpenH264RunOpts {
            output_index,
            fps: software_fps,
            bitrate_kbps,
            framed,
            adapter_hint: adapter_hint.clone(),
            adapter_output_index,
            device_name: device_name.clone(),
            cursor_mode,
            color,
        };
        if let Some(probe) = admission_probe.as_ref() {
            std::process::exit(crate::win_mf::run_openh264_admission_probe(options, probe));
        }
        crate::win_mf::run_openh264(options);
    }

    // ---- Explicit MF request short-circuits before touching NVENC state. ----
    #[cfg(feature = "mf")]
    if encoder_choice == "mf" {
        if !codec.eq_ignore_ascii_case("h264")
            || color.chroma != arcen_media::ChromaSubsampling::Yuv420
            || color.bit_depth != arcen_media::BitDepth::Eight
        {
            log("MF software encoding requires h264 + yuv420 8-bit");
            std::process::exit(2);
        }
        let options = crate::win_mf::MfRunOpts {
            output_index,
            fps: software_fps,
            bitrate_kbps,
            profile: crate::mf_encoder::H264Profile::Main,
            gop_secs: 2,
            framed,
            adapter_hint: adapter_hint.clone(),
            adapter_output_index,
            device_name: device_name.clone(),
            cursor_mode,
            color,
        };
        if let Some(probe) = admission_probe.as_ref() {
            std::process::exit(crate::win_mf::run_admission_probe(options, probe));
        }
        crate::win_mf::run(options);
    }

    // Keep the (virtual) display AWAKE. On a headless VM/vGPU host Windows
    // sleeps the idle display, which SUSPENDS DWM composition -> DXGI Desktop
    // Duplication times out forever. ES_CONTINUOUS holds the request for the
    // process lifetime; ES_DISPLAY_REQUIRED keeps the monitor powered so DWM
    // keeps compositing. This is what makes headless capture work WITHOUT a
    // third-party virtual-display driver on cards that already expose a
    // virtual head (Quadro/GRID vGPU).
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
    }
    log("display keep-awake armed (ES_DISPLAY_REQUIRED|ES_CONTINUOUS)");

    #[cfg(feature = "nvenc")]
    if let Some(options) = admission_probe.as_ref() {
        if encoder_choice != "nvenc" {
            log("admission probe requires a concrete encoder selection");
            std::process::exit(2);
        }
        let (device, context) = match unsafe { create_encode_device(&selector) } {
            Ok(device) => device,
            Err(error) => {
                log(&format!("admission probe device init failed: {error:?}"));
                std::process::exit(2);
            }
        };
        let code =
            unsafe { run_admission_probe(device, context, &codec, color, qp_map_policy, options) };
        std::process::exit(code);
    }

    // Selftest short-circuit: validate the NVENC hot-path with synthetic content
    // and NO desktop, so it runs on a headless / session-0 context (e.g. over
    // SSH) where Desktop Duplication is unavailable. Must run BEFORE Capture::new.
    #[cfg(feature = "nvenc")]
    if selftest {
        let (device, context) = match unsafe { create_headless_selftest_device() } {
            Ok(dc) => dc,
            Err(e) => {
                log(&format!("selftest device init failed: {e:?}"));
                std::process::exit(2);
            }
        };
        let code = unsafe {
            run_selftest(
                device,
                context,
                &codec,
                st_w,
                st_h,
                color,
                qp_map_policy,
                framed,
            )
        };
        std::process::exit(code);
    }

    // ---- Live capture path ----
    // NVENC build: bind capability checks to the adapter that owns the selected
    // output. A globally installed NVIDIA runtime must never make `auto` choose
    // NVENC for an Intel/AMD/VMware desktop.
    #[cfg(feature = "nvenc")]
    {
        if encoder_choice == "auto" || encoder_choice == "nvenc" {
            // Explicit `nvenc` fails closed at every rung; `auto` degrades to
            // the software fallback instead of exiting, because auto's whole job is
            // to keep the session alive on whatever the adapter can do.
            let selected_vendor = match unsafe { Capture::find_output_device(&selector) } {
                Ok(found) => {
                    log(&format!(
                        "selected capture target: {} -> adapter {} ({:?}) vendor=0x{:04x}",
                        selector.describe(),
                        found.adapter_index,
                        found.adapter_name,
                        found.vendor_id
                    ));
                    found.vendor_id
                }
                Err(error) => {
                    log(&format!("selected output adapter probe failed: {error:?}"));
                    std::process::exit(2);
                }
            };
            let can_try_nvenc = selected_vendor == 0x10de;
            if can_try_nvenc {
                let force_wgc = args.iter().any(|a| a == "wgc");
                let force_dda = args.iter().any(|a| a == "ddapi");
                // Every stream above 8-bit needs an FP16 WGC source; otherwise
                // an 8-bit BGRA desktop would merely be repacked into a deeper
                // container. HDR additionally requires proof that DWM is
                // compositing the selected output as PQ/BT.2020.
                //
                // An HDR *request* is not an HDR *desktop*. The EDID
                // makes Windows offer HDR; only `advancedColorEnabled`
                // makes DWM composite in FP16 scRGB. Enable it here, read
                // it back, and if it did not engage, stop claiming PQ --
                // a wide pool over an SDR desktop signalled as PQ tells the
                // Deck to tone-map SDR against a 1000-nit curve, which is
                // worse than never asking for HDR at all.
                let wide_capture = wide_capture_required(color.bit_depth);
                let hdr_required = hdr_output_required(color.transfer);
                let source = unsafe {
                    select_source(
                        &selector,
                        force_wgc,
                        force_dda,
                        cursor_mode,
                        wide_capture,
                        hdr_required,
                    )
                };
                match unsafe { create_nvenc_encoder(&source, &codec, color, intent, qp_map_policy) }
                {
                    Ok(mut encoder) => unsafe {
                        // Construction truthfully records whether this selected
                        // policy received a DELTA-capability trial. Engagement
                        // additionally needs the concrete capture format.
                        if qp_map_policy.submits_map() {
                            let engaged = encoder.enable_qp_map(
                                qp_map_policy,
                                arcen_media::video::QpBias::default(),
                                arcen_media::VideoCodec::from_token(&codec)
                                    .unwrap_or(arcen_media::VideoCodec::H264),
                            );
                            log(&format!(
                                "QP map policy={} engaged={engaged}",
                                qp_map_policy.token()
                            ));
                        }
                        let code =
                            run_encode(source, encoder, &codec, fps, color, framed, cursor_mode);
                        std::process::exit(code);
                    },
                    Err(error) => {
                        log(&format!("NVENC init failed: {error}"));
                        if encoder_choice == "nvenc" || !error.allows_software_fallback() {
                            std::process::exit(4);
                        }
                        let Some(reason) = error.unavailable_reason() else {
                            log("NVENC fallback classification lost its typed reason");
                            std::process::exit(4);
                        };
                        let _ = crate::announce_unavailable(
                            arcen_media::video::EncoderBackend::NativeNvenc,
                            reason,
                        );
                        log(
                            "auto: parent must retarget display geometry before the OpenH264 fallback",
                        );
                        drop(source);
                        std::process::exit(6);
                    }
                }
            } else {
                log(&format!(
                    "NVENC unavailable on selected non-NVIDIA adapter vendor=0x{selected_vendor:04x}",
                ));
                if encoder_choice == "nvenc" {
                    std::process::exit(6);
                }
            }

            if !codec.eq_ignore_ascii_case("h264")
                || color.chroma != arcen_media::ChromaSubsampling::Yuv420
                || color.bit_depth != arcen_media::BitDepth::Eight
            {
                log(
                    "auto cannot fall back to OpenH264: software fallback requires h264 + yuv420 8-bit",
                );
                std::process::exit(6);
            }
            #[cfg(feature = "software-h264")]
            {
                log("falling back to source-built OpenH264");
                crate::win_mf::run_openh264(crate::win_mf::OpenH264RunOpts {
                    output_index,
                    fps: software_fps,
                    bitrate_kbps,
                    framed,
                    adapter_hint: adapter_hint.clone(),
                    adapter_output_index,
                    device_name: device_name.clone(),
                    cursor_mode,
                    color,
                });
            }
            #[cfg(all(not(feature = "software-h264"), feature = "mf"))]
            {
                log("falling back to Media Foundation SW H.264 encoder");
                crate::win_mf::run(crate::win_mf::MfRunOpts {
                    output_index,
                    fps: software_fps,
                    bitrate_kbps,
                    profile: crate::mf_encoder::H264Profile::Main,
                    gop_secs: 2,
                    framed,
                    adapter_hint: adapter_hint.clone(),
                    adapter_output_index,
                    device_name: device_name.clone(),
                    cursor_mode,
                    color,
                });
            }
            #[cfg(not(any(feature = "software-h264", feature = "mf")))]
            {
                log("no compatible fallback compiled — exiting");
                std::process::exit(6);
            }
        } else {
            log(&format!("unknown encoder selection: {encoder_choice:?}"));
            std::process::exit(2);
        }
    }

    // Capture-only build (no NVENC): DXGI Desktop Duplication measurement loop
    // (retained for the historical Linux-first CI target) or software fallback.
    #[cfg(not(feature = "nvenc"))]
    {
        #[cfg(feature = "software-h264")]
        {
            if encoder_choice != "auto" {
                log(&format!("unknown encoder selection: {encoder_choice:?}"));
                std::process::exit(2);
            }
            if !codec.eq_ignore_ascii_case("h264")
                || color.chroma != arcen_media::ChromaSubsampling::Yuv420
                || color.bit_depth != arcen_media::BitDepth::Eight
            {
                log("OpenH264 software encoding requires h264 + yuv420 8-bit");
                std::process::exit(2);
            }
            crate::win_mf::run_openh264(crate::win_mf::OpenH264RunOpts {
                output_index,
                fps: software_fps,
                bitrate_kbps,
                framed,
                adapter_hint,
                adapter_output_index,
                device_name,
                cursor_mode,
                color,
            });
        }
        #[cfg(all(not(feature = "software-h264"), feature = "mf"))]
        {
            if encoder_choice == "nvenc" {
                log("NVENC requested but this capenc build has no NVENC backend");
                std::process::exit(6);
            }
            if encoder_choice != "auto" && encoder_choice != "mf" {
                log(&format!("unknown encoder selection: {encoder_choice:?}"));
                std::process::exit(2);
            }
            if !codec.eq_ignore_ascii_case("h264")
                || color.chroma != arcen_media::ChromaSubsampling::Yuv420
                || color.bit_depth != arcen_media::BitDepth::Eight
            {
                log("MF software encoding requires h264 + yuv420 8-bit");
                std::process::exit(2);
            }
            crate::win_mf::run(crate::win_mf::MfRunOpts {
                output_index,
                fps: software_fps,
                bitrate_kbps,
                profile: crate::mf_encoder::H264Profile::Main,
                gop_secs: 2,
                framed,
                adapter_hint,
                adapter_output_index,
                device_name,
                cursor_mode,
                color,
            });
        }
        #[cfg(not(any(feature = "software-h264", feature = "mf")))]
        {
            let cap = match unsafe { Capture::new(&selector) } {
                Ok(c) => c,
                Err(e) => {
                    log(&format!("init failed: {e:?} (session 0 cannot capture)"));
                    std::process::exit(2);
                }
            };
            log(&format!(
                "DXGI ready: {}x{} {}",
                cap.width,
                cap.height,
                selector.describe()
            ));
            run_capture_only(cap)
        }
    }
}

#[cfg(test)]
mod selector_tests {
    //! Pure-logic tests for `OutputSelector`/`resolve_selector`: no live DXGI
    //! adapter is needed since `OutputCandidate` is plain data, matching this
    //! crate's actual enumeration output field-for-field.
    #[cfg(feature = "nvenc")]
    use super::{hdr_output_required, wgc_requirement, wide_capture_required};
    use super::{resolve_selector, OutputCandidate, OutputSelector, SelectorResolution};

    fn candidate(
        attached_global_index: u32,
        adapter_name: &str,
        local_index: u32,
        device_name: &str,
    ) -> OutputCandidate {
        OutputCandidate {
            attached_global_index,
            adapter_name: adapter_name.to_string(),
            local_index,
            device_name: device_name.to_string(),
        }
    }

    fn legacy_selector(global_output_index: u32) -> OutputSelector<'static> {
        OutputSelector {
            global_output_index,
            adapter_hint: None,
            adapter_output_index: None,
            device_name: None,
        }
    }

    #[cfg(feature = "nvenc")]
    #[test]
    fn wide_capture_always_requires_wgc_even_with_a_local_cursor() {
        assert_eq!(
            wgc_requirement(false, crate::CursorCaptureMode::Local, true),
            Some("capture backend: WGC (required by FP16 wide capture)")
        );
        assert!(wgc_requirement(false, crate::CursorCaptureMode::Local, false).is_none());
    }

    #[cfg(feature = "nvenc")]
    #[test]
    fn ten_bit_sdr_needs_wide_capture_but_not_hdr_output_state() {
        assert!(wide_capture_required(arcen_media::BitDepth::Ten));
        assert!(!hdr_output_required(
            arcen_media::TransferCharacteristics::Bt709
        ));
    }

    #[cfg(feature = "nvenc")]
    #[test]
    fn pq_needs_both_wide_capture_and_hdr_output_state() {
        assert!(wide_capture_required(arcen_media::BitDepth::Ten));
        assert!(hdr_output_required(
            arcen_media::TransferCharacteristics::Pq
        ));
    }

    #[test]
    fn legacy_output_index_path_is_unchanged_when_no_explicit_selector_is_given() {
        let candidates = vec![
            candidate(0, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1"),
            candidate(1, "NVIDIA GeForce RTX 4090", 1, r"\\.\DISPLAY2"),
        ];
        assert_eq!(
            resolve_selector(&legacy_selector(0), &candidates),
            SelectorResolution::Resolved(0)
        );
        assert_eq!(
            resolve_selector(&legacy_selector(1), &candidates),
            SelectorResolution::Resolved(1)
        );
        // Out-of-range global index fails closed rather than clamping/wrapping.
        assert_eq!(
            resolve_selector(&legacy_selector(2), &candidates),
            SelectorResolution::Missing
        );
    }

    #[test]
    fn resolves_a_non_contiguous_attached_global_index() {
        // Attached global indices are assigned only to desktop-attached
        // outputs, so an inactive output between two active ones does not
        // reserve a slot — 0, 1, 2 here may correspond to non-contiguous
        // adapter-local output numbering upstream, but the exposed global
        // index itself is exactly what the pier's fresh inventory expects to
        // match by when no explicit selector narrows further.
        let candidates = vec![
            candidate(0, "Adapter A", 0, r"\\.\DISPLAY1"),
            candidate(1, "Adapter A", 2, r"\\.\DISPLAY3"),
            candidate(2, "Adapter B", 0, r"\\.\DISPLAY4"),
        ];
        assert_eq!(
            resolve_selector(&legacy_selector(1), &candidates),
            SelectorResolution::Resolved(1)
        );
        assert_eq!(candidates[1], candidate(1, "Adapter A", 2, r"\\.\DISPLAY3"));
    }

    #[test]
    fn resolves_correctly_across_identically_named_adapters_using_device_name() {
        // Two physically distinct adapters sharing a model string: an
        // adapter-name-only selector would be ambiguous, but `device=`
        // (the globally unique `\\.\DISPLAYn` string) disambiguates.
        let candidates = vec![
            candidate(0, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1"),
            candidate(1, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY2"),
        ];
        let selector = OutputSelector {
            global_output_index: 0,
            adapter_hint: Some("NVIDIA GeForce RTX 4090"),
            adapter_output_index: Some(0),
            device_name: Some(r"\\.\DISPLAY2"),
        };
        assert_eq!(
            resolve_selector(&selector, &candidates),
            SelectorResolution::Resolved(1)
        );
    }

    #[test]
    fn device_name_alone_resolves_uniquely_without_an_adapter_hint() {
        let candidates = vec![
            candidate(0, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1"),
            candidate(1, "AMD Radeon Pro W6800", 0, r"\\.\DISPLAY2"),
        ];
        let selector = OutputSelector {
            global_output_index: 0,
            adapter_hint: None,
            adapter_output_index: None,
            device_name: Some(r"\\.\DISPLAY2"),
        };
        assert_eq!(
            resolve_selector(&selector, &candidates),
            SelectorResolution::Resolved(1)
        );
    }

    #[test]
    fn fails_ambiguous_when_adapter_name_and_local_index_match_two_candidates_with_no_device_name()
    {
        // Same model name, same adapter-local output index, on two distinct
        // physical adapters — a real (if unusual) multi-GPU shape. With no
        // `device=` to disambiguate, this must fail closed, never silently
        // pick the first match.
        let candidates = vec![
            candidate(0, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1"),
            candidate(1, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY2"),
        ];
        let selector = OutputSelector {
            global_output_index: 0,
            adapter_hint: Some("NVIDIA GeForce RTX 4090"),
            adapter_output_index: Some(0),
            device_name: None,
        };
        assert_eq!(
            resolve_selector(&selector, &candidates),
            SelectorResolution::Ambiguous(2)
        );
    }

    #[test]
    fn fails_closed_when_the_explicit_selector_matches_no_candidate() {
        let candidates = vec![candidate(0, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1")];
        let selector = OutputSelector {
            global_output_index: 0,
            adapter_hint: Some("NVIDIA GeForce RTX 4090"),
            adapter_output_index: Some(0),
            device_name: Some(r"\\.\DISPLAY9"),
        };
        assert_eq!(
            resolve_selector(&selector, &candidates),
            SelectorResolution::Missing
        );
    }

    #[test]
    fn re_enumeration_after_unplug_fails_closed_instead_of_reusing_a_stale_index() {
        // Simulates the child re-enumerating (as it always does, once per
        // process start/restart) between the pier's earlier probe and this
        // spawn: the previously bound output is gone from the fresh list.
        let selector = OutputSelector {
            global_output_index: 0,
            adapter_hint: Some("NVIDIA GeForce RTX 4090"),
            adapter_output_index: Some(1),
            device_name: Some(r"\\.\DISPLAY2"),
        };
        let before_unplug = vec![
            candidate(0, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1"),
            candidate(1, "NVIDIA GeForce RTX 4090", 1, r"\\.\DISPLAY2"),
        ];
        assert_eq!(
            resolve_selector(&selector, &before_unplug),
            SelectorResolution::Resolved(1)
        );
        let after_unplug = vec![candidate(0, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1")];
        assert_eq!(
            resolve_selector(&selector, &after_unplug),
            SelectorResolution::Missing
        );
    }

    #[test]
    fn re_enumeration_after_replug_resolves_the_new_current_index_not_the_stale_one() {
        // The bound monitor comes back attached to a different adapter-local
        // output number (and thus a different global index) after replugging
        // — the fresh enumeration must be trusted, never a cached position.
        let selector = OutputSelector {
            global_output_index: 0,
            adapter_hint: Some("NVIDIA GeForce RTX 4090"),
            adapter_output_index: None,
            device_name: Some(r"\\.\DISPLAY2"),
        };
        let before = vec![
            candidate(0, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1"),
            candidate(1, "NVIDIA GeForce RTX 4090", 1, r"\\.\DISPLAY2"),
        ];
        assert_eq!(
            resolve_selector(&selector, &before),
            SelectorResolution::Resolved(1)
        );
        let after_replug = vec![
            candidate(0, "NVIDIA GeForce RTX 4090", 2, r"\\.\DISPLAY2"),
            candidate(1, "NVIDIA GeForce RTX 4090", 0, r"\\.\DISPLAY1"),
        ];
        assert_eq!(
            resolve_selector(&selector, &after_replug),
            SelectorResolution::Resolved(0)
        );
    }

    #[test]
    fn has_explicit_selector_is_false_only_when_every_field_is_none() {
        assert!(!legacy_selector(0).has_explicit_selector());
        assert!(OutputSelector {
            global_output_index: 0,
            adapter_hint: Some("x"),
            adapter_output_index: None,
            device_name: None,
        }
        .has_explicit_selector());
        assert!(OutputSelector {
            global_output_index: 0,
            adapter_hint: None,
            adapter_output_index: Some(1),
            device_name: None,
        }
        .has_explicit_selector());
        assert!(OutputSelector {
            global_output_index: 0,
            adapter_hint: None,
            adapter_output_index: None,
            device_name: Some("x"),
        }
        .has_explicit_selector());
    }

    #[test]
    fn describe_reports_output_index_only_for_the_legacy_shape() {
        assert_eq!(legacy_selector(3).describe(), "output_index=3");
        let explicit = OutputSelector {
            global_output_index: 3,
            adapter_hint: Some("NVIDIA GeForce RTX 4090"),
            adapter_output_index: Some(1),
            device_name: Some(r"\\.\DISPLAY2"),
        };
        assert_eq!(
            explicit.describe(),
            r"adapter=NVIDIA GeForce RTX 4090 adapter-output=1 device=\\.\DISPLAY2"
        );
    }
}
