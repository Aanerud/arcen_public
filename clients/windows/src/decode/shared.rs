// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Decoding on a D3D11 device of its own, so that no presenter's `Present`
//! (or anything else on a presenter's device) can stall decode.
//!
//! The decoder decodes into its own texture array, as always, then copies
//! each picture it outputs into one texture of a small pool of shareable
//! textures (`D3D11_RESOURCE_MISC_SHARED | SHARED_NTHANDLE`, one picture
//! each, never the decoder's array: sharing decoder arrays fails on some
//! drivers) and signals a shared fence. A presenter on another device opens
//! the texture and the fence through their NT handles, waits GPU-side for
//! the fence before it samples (decode → render), and signals a fence of
//! its own once its reads are queued; the decoder waits GPU-side for that
//! fence before it writes the texture again (render → decode). Nothing in
//! between waits on the CPU.
//!
//! A texture is written again only once every [`SharedPicture`] clone of
//! it is gone (the pool's leases) and the readers' fences are passed; the
//! pool is bounded: when every texture is still held, the newest picture is
//! dropped and counted, never waited for (the presenters keep only the
//! newest picture per monitor anyway).
//!
//! The hand-over is API-neutral on the reading side: a texture NT handle, a
//! fence NT handle and a value. A later D3D12 video decoder produces the
//! same (`D3D12_HEAP_FLAG_SHARED` resources, `D3D12_FENCE_FLAG_SHARED`
//! fences), and the presenters need no change.
//!
//! Whether a device of its own is used is decided per connection from
//! [`prove_sharing`]: the shared qualification rule
//! (`arcen_deck_core::decode::decode_device_split`: extended resource
//! sharing, shareable fences, and only Intel, NVIDIA from driver 471.11 and
//! AMD from Vega, where other streaming clients found shared decode
//! textures reliable), shared fences both ways, and per output format a
//! real keyframe handed over and read back sample-exact on the presenter's
//! device. A connection whose requested picture needs a format that is not
//! proven decodes on the presenter's device as before; a decoder never
//! hands over a format that is not proven.

#![allow(
    unsafe_code,
    reason = "D3D11 and DXGI are COM APIs; every call is an FFI call on live interfaces"
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use arcen_deck_core::decode::{DecodeDeviceFacts, decode_device_split};
use arcen_media::bitstream::picture::Slot;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_FEATURE_D3D11_OPTIONS, D3D11_FEATURE_DATA_D3D11_OPTIONS,
    D3D11_FEATURE_DATA_SHADER_MIN_PRECISION_SUPPORT, D3D11_FEATURE_SHADER_MIN_PRECISION_SUPPORT,
    D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_SHADER_MIN_PRECISION_16_BIT, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11Device1, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use arcen_media::annexb::{NalCodec, split_access_units};
use arcen_media::bitstream::picture::StreamCodecKind;

use super::backend::D3d11VideoDecoder;
use super::device::{DecoderProfile, PROFILES, VideoDevice};
use super::probe::{PROBE_KEYFRAMES, fnv1a};
use super::readback::read_planar;
use super::session::{D3d11Picture, SlotLease, SlotLeases};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An NT handle this process created (unnamed, default security: usable
/// only inside this process); closed when the last user drops it.
#[derive(Debug)]
pub struct SharedHandle(HANDLE);

impl SharedHandle {
    #[must_use]
    pub const fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for SharedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was created by this process and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// SAFETY: an NT handle value is a process-wide token; the objects it names
// are free-threaded.
unsafe impl Send for SharedHandle {}
// SAFETY: as above; the handle is only read.
unsafe impl Sync for SharedHandle {}

/// A GPU fence shared between devices: its identity (devices cache what
/// they opened) and its NT handle.
#[derive(Debug, Clone)]
pub struct SharedFence {
    pub id: u64,
    pub handle: Arc<SharedHandle>,
}

impl SharedFence {
    /// A new shared fence on `device`, starting at 0.
    ///
    /// # Errors
    ///
    /// The device has no shareable fences (D3D11.4 monitored fences).
    pub fn create(device: &ID3D11Device) -> Result<(ID3D11Fence, Self), String> {
        let device5: ID3D11Device5 = device
            .cast()
            .map_err(|error| format!("no ID3D11Device5 (fences): {error}"))?;
        // SAFETY: owned out-parameter; the handle is owned by `SharedHandle`.
        unsafe {
            let mut fence: Option<ID3D11Fence> = None;
            device5
                .CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)
                .map_err(|error| format!("CreateFence(shared): {error}"))?;
            let fence = fence.ok_or("CreateFence(shared): none returned")?;
            let handle = fence
                .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                .map_err(|error| format!("fence CreateSharedHandle: {error}"))?;
            Ok((
                fence,
                Self {
                    id: next_id(),
                    handle: Arc::new(SharedHandle(handle)),
                },
            ))
        }
    }

    /// Opens this fence on `device`.
    ///
    /// # Errors
    ///
    /// The device cannot open shared fences.
    pub fn open(&self, device: &ID3D11Device) -> Result<ID3D11Fence, String> {
        let device5: ID3D11Device5 = device
            .cast()
            .map_err(|error| format!("no ID3D11Device5 (fences): {error}"))?;
        // SAFETY: the handle is live (held by `self`); owned out-parameter.
        unsafe {
            let mut fence: Option<ID3D11Fence> = None;
            device5
                .OpenSharedFence(self.handle.raw(), &mut fence)
                .map_err(|error| format!("OpenSharedFence: {error}"))?;
            fence.ok_or_else(|| "OpenSharedFence: none returned".to_owned())
        }
    }
}

/// The readers of one pool texture: each presenter's release fence and the
/// value it signals once its reads of the texture are queued before it.
#[derive(Debug, Default)]
pub struct ReleaseFences {
    entries: Mutex<Vec<(SharedFence, u64)>>,
}

impl ReleaseFences {
    /// A presenter's reads end when `fence` reaches `value`.
    pub fn record(&self, fence: &SharedFence, value: u64) {
        let mut entries = lock(&self.entries);
        if let Some(entry) = entries.iter_mut().find(|(known, _)| known.id == fence.id) {
            entry.1 = entry.1.max(value);
        } else {
            entries.push((fence.clone(), value));
        }
    }

    fn take(&self) -> Vec<(SharedFence, u64)> {
        std::mem::take(&mut *lock(&self.entries))
    }
}

/// What a presenter on another device needs to read a decoded picture.
#[derive(Debug, Clone)]
pub struct SharedPicture {
    /// Identifies the shared texture (presenters cache what they opened).
    pub texture_id: u64,
    /// Identifies the pool it belongs to: a presenter keeps only the
    /// current pool's textures open (an open texture keeps it alive).
    pub pool_id: u64,
    pub texture: Arc<SharedHandle>,
    /// Wait GPU-side for `ready` to reach `ready_value` before sampling.
    pub ready: SharedFence,
    pub ready_value: u64,
    /// Record the presenter's release fence here after its reads.
    pub released: Arc<ReleaseFences>,
}

/// Creates a shareable one-picture texture and its NT handle.
fn shared_texture(
    device: &ID3D11Device,
    format: DXGI_FORMAT,
    (width, height): (u32, u32),
) -> Result<(ID3D11Texture2D, SharedHandle), String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: u32::try_from(D3D11_BIND_SHADER_RESOURCE.0).unwrap_or(0),
        CPUAccessFlags: 0,
        MiscFlags: u32::try_from(
            D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0,
        )
        .unwrap_or(0),
    };
    // SAFETY: the descriptor outlives the call; owned out-parameters.
    unsafe {
        let mut texture = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|error| format!("shared {format:?} {width}x{height} texture: {error}"))?;
        let texture: ID3D11Texture2D = texture.ok_or("shared texture: none returned")?;
        let resource: IDXGIResource1 = texture
            .cast()
            .map_err(|error| format!("IDXGIResource1: {error}"))?;
        let handle = resource
            .CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                PCWSTR::null(),
            )
            .map_err(|error| format!("texture CreateSharedHandle({format:?}): {error}"))?;
        Ok((texture, SharedHandle(handle)))
    }
}

/// Opens a shared texture on `device`.
///
/// # Errors
///
/// The device cannot open it (format or driver).
pub fn open_texture(
    device: &ID3D11Device,
    handle: &SharedHandle,
) -> Result<ID3D11Texture2D, String> {
    let device1: ID3D11Device1 = device
        .cast()
        .map_err(|error| format!("no ID3D11Device1: {error}"))?;
    // SAFETY: the handle is live (held by the caller).
    unsafe { device1.OpenSharedResource1(handle.raw()) }
        .map_err(|error| format!("OpenSharedResource1: {error}"))
}

/// What [`prove_sharing`] proved for one adapter: the decoder output
/// formats a presenter on another device reads sample-exact, and the ones
/// it does not (with why).
#[derive(Debug, Clone, Default)]
pub struct ShareProof {
    pub formats: Vec<DXGI_FORMAT>,
    pub refused: Vec<(DXGI_FORMAT, String)>,
    /// Vendor and driver, for the log.
    pub driver: String,
}

/// A decoder output format's name, for the log.
#[must_use]
pub fn format_name(format: DXGI_FORMAT) -> String {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_Y410,
    };
    match format {
        DXGI_FORMAT_NV12 => "NV12".to_owned(),
        DXGI_FORMAT_P010 => "P010".to_owned(),
        DXGI_FORMAT_Y410 => "Y410".to_owned(),
        DXGI_FORMAT_AYUV => "AYUV".to_owned(),
        other => format!("{other:?}"),
    }
}

impl ShareProof {
    /// The proven formats and the refused ones, for the log.
    #[must_use]
    pub fn describe(&self) -> String {
        let proven = self
            .formats
            .iter()
            .map(|format| format_name(*format))
            .collect::<Vec<_>>()
            .join(", ");
        let refused = self
            .refused
            .iter()
            .map(|(format, why)| format!("{} ({why})", format_name(*format)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "shared: [{proven}]; not shared: [{refused}]; {}",
            self.driver
        )
    }
}

/// One adapter's [`prove_sharing`] answer.
type Proven = Result<ShareProof, String>;

/// Whether pictures decoded on `decode` can be handed to a presenter on
/// `presenter` without sharing a device, and in which formats: the shared
/// rule ([`arcen_deck_core::decode::decode_device_split`]: extended
/// resource sharing, shareable fences and a qualified adapter and driver),
/// shared fences both ways in fact, and, for every output format of a
/// profile this adapter decodes, a real probe keyframe decoded here, handed
/// over and read back sample-exact on `presenter` (a driver can open a
/// shared texture and still read it differently: the lab's virtual GPU does
/// with `Y410`). The answer is kept per adapter for the process.
///
/// # Errors
///
/// Why the adapter does not qualify at all, for the log.
pub fn prove_sharing(
    decode: &VideoDevice,
    presenter: &ID3D11Device,
    presenter_lock: &Mutex<()>,
    luid: windows::Win32::Foundation::LUID,
) -> Result<ShareProof, String> {
    static PROOFS: std::sync::OnceLock<Mutex<HashMap<(i32, u32), Proven>>> =
        std::sync::OnceLock::new();
    let key = (luid.HighPart, luid.LowPart);
    let proofs = PROOFS.get_or_init(Mutex::default);
    if let Some(known) = lock(proofs).get(&key) {
        return known.clone();
    }
    let proof = prove_sharing_now(decode, presenter, presenter_lock);
    lock(proofs).insert(key, proof.clone());
    proof
}

fn prove_sharing_now(
    decode: &VideoDevice,
    presenter: &ID3D11Device,
    presenter_lock: &Mutex<()>,
) -> Result<ShareProof, String> {
    let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS::default();
    let mut precision = D3D11_FEATURE_DATA_SHADER_MIN_PRECISION_SUPPORT::default();
    // SAFETY: the out-parameters are the documented structures, sized exactly.
    unsafe {
        decode
            .device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D11_OPTIONS,
                std::ptr::from_mut(&mut options).cast(),
                u32::try_from(std::mem::size_of::<D3D11_FEATURE_DATA_D3D11_OPTIONS>()).unwrap_or(0),
            )
            .map_err(|error| format!("D3D11_OPTIONS: {error}"))?;
        let _ = decode.device.CheckFeatureSupport(
            D3D11_FEATURE_SHADER_MIN_PRECISION_SUPPORT,
            std::ptr::from_mut(&mut precision).cast(),
            u32::try_from(std::mem::size_of::<
                D3D11_FEATURE_DATA_SHADER_MIN_PRECISION_SUPPORT,
            >())
            .unwrap_or(0),
        );
    }
    let fences = SharedFence::create(&decode.device)
        .and_then(|(_, ready)| ready.open(presenter))
        .and_then(|_| SharedFence::create(presenter))
        .and_then(|(_, release)| release.open(&decode.device));
    let facts = DecodeDeviceFacts {
        vendor_id: decode.vendor_id,
        driver_version: decode.driver_version,
        extended_resource_sharing: options.ExtendedResourceSharing.as_bool(),
        shared_fences: fences.is_ok(),
        min_precision_16_bit: precision.PixelShaderMinPrecision
            & u32::try_from(D3D11_SHADER_MIN_PRECISION_16_BIT.0).unwrap_or(0)
            != 0,
    };
    let driver = format!(
        "vendor {:04x}, driver {}",
        decode.vendor_id,
        decode.driver_version.map_or_else(
            || "unknown".to_owned(),
            |[a, b, c, d]| format!("{a}.{b}.{c}.{d}"),
        )
    );
    if let Err(reason) = decode_device_split(&facts) {
        let detail = fences
            .err()
            .map(|error| format!(" ({error})"))
            .unwrap_or_default();
        return Err(format!("{reason}{detail}; {driver}"));
    }
    let mut proof = ShareProof {
        driver,
        ..ShareProof::default()
    };
    for profile in PROFILES {
        let known = proof.formats.contains(&profile.output)
            || proof
                .refused
                .iter()
                .any(|(format, _)| *format == profile.output);
        if known || decode.probe(&profile).is_err() {
            continue;
        }
        match prove_format(decode, presenter, presenter_lock, &profile) {
            Ok(()) => proof.formats.push(profile.output),
            Err(why) => proof.refused.push((profile.output, why)),
        }
    }
    Ok(proof)
}

/// Decodes `profile`'s probe keyframe on `decode`, hands it over and reads
/// it back on `presenter`: it must be the same picture, sample for sample.
fn prove_format(
    decode: &VideoDevice,
    presenter: &ID3D11Device,
    presenter_lock: &Mutex<()>,
    profile: &DecoderProfile,
) -> Result<(), String> {
    let keyframe = PROBE_KEYFRAMES
        .iter()
        .find(|keyframe| keyframe.profile.output == profile.output)
        .ok_or("no probe keyframe")?;
    let nal = match keyframe.codec {
        StreamCodecKind::H264 => NalCodec::H264,
        StreamCodecKind::H265 => NalCodec::H265,
    };
    let unit = split_access_units(nal, keyframe.stream)
        .into_iter()
        .next()
        .ok_or("probe stream has no access unit")?;
    let mut decoder = D3d11VideoDecoder::exporting(decode.clone(), None);
    let picture = decoder
        .decode_annex_b(keyframe.codec, unit)
        .map_err(|error| format!("probe keyframe: {error}"))?
        .pop()
        .ok_or("probe keyframe produced no picture")?;
    let shared = picture.shared.as_ref().ok_or("not handed over")?;
    let texture = open_texture(presenter, &shared.texture)?;
    let ready = shared.ready.open(presenter)?;
    {
        let _context = lock(presenter_lock);
        // SAFETY: the immediate context of a live device; the fence is live.
        unsafe {
            let context: ID3D11DeviceContext4 = presenter
                .GetImmediateContext()
                .and_then(|context| context.cast())
                .map_err(|error| format!("ID3D11DeviceContext4: {error}"))?;
            context
                .Wait(&ready, shared.ready_value)
                .map_err(|error| format!("Wait: {error}"))?;
        }
    }
    let local = D3d11Picture {
        texture,
        fence: None,
        shared: None,
        ..picture.clone()
    };
    let planar = read_planar(presenter, presenter_lock, &local)?;
    if fnv1a(&planar) == keyframe.fnv {
        Ok(())
    } else {
        Err("another device reads it differently".into())
    }
}

struct PoolTexture {
    id: u64,
    texture: ID3D11Texture2D,
    handle: Arc<SharedHandle>,
    released: Arc<ReleaseFences>,
}

struct Pool {
    id: u64,
    format: DXGI_FORMAT,
    size: (u32, u32),
    textures: Vec<PoolTexture>,
    leases: Arc<SlotLeases>,
    next: usize,
}

/// Where the hand-over's time goes, logged with the decoder timing every
/// five seconds.
struct ExportTiming {
    since: Instant,
    exports: u32,
    reader_waits: u32,
    exhausted: u32,
    total: Duration,
    total_max: Duration,
}

impl ExportTiming {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            exports: 0,
            reader_waits: 0,
            exhausted: 0,
            total: Duration::ZERO,
            total_max: Duration::ZERO,
        }
    }
}

/// The decoder's side of the hand-over (one per decoder, on the decoder's
/// own device).
pub struct SharedExport {
    device: ID3D11Device,
    context: ID3D11DeviceContext4,
    context_lock: Arc<Mutex<()>>,
    ready: ID3D11Fence,
    ready_shared: SharedFence,
    ready_value: u64,
    pool: Option<Pool>,
    pool_size: u8,
    /// Presenters' release fences, opened on the decoder's device.
    readers: HashMap<u64, ID3D11Fence>,
    decoder: u64,
    timing: ExportTiming,
}

impl std::fmt::Debug for SharedExport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedExport")
            .field("pool_size", &self.pool_size)
            .field("ready_value", &self.ready_value)
            .finish_non_exhaustive()
    }
}

impl SharedExport {
    /// The hand-over for one decoder on `device` with `pool_size` textures.
    ///
    /// # Errors
    ///
    /// No shared fence on this device.
    pub fn new(device: &VideoDevice, pool_size: u8, decoder: u64) -> Result<Self, String> {
        let (ready, ready_shared) = SharedFence::create(&device.device)?;
        // SAFETY: the immediate context of a live device.
        let context: ID3D11DeviceContext4 = unsafe { device.device.GetImmediateContext() }
            .and_then(|context| context.cast())
            .map_err(|error| format!("ID3D11DeviceContext4: {error}"))?;
        Ok(Self {
            device: device.device.clone(),
            context,
            context_lock: Arc::clone(&device.context_lock),
            ready,
            ready_shared,
            ready_value: 0,
            pool: None,
            pool_size: pool_size.max(2),
            readers: HashMap::new(),
            decoder,
            timing: ExportTiming::new(),
        })
    }

    fn pool_for(&mut self, format: DXGI_FORMAT, size: (u32, u32)) -> Result<&Pool, String> {
        if self
            .pool
            .as_ref()
            .is_none_or(|pool| pool.format != format || pool.size != size)
        {
            // The old pool's textures stay alive while presenters hold
            // pictures of them.
            self.pool = None;
            let mut textures = Vec::with_capacity(usize::from(self.pool_size));
            for _ in 0..self.pool_size {
                let (texture, handle) = shared_texture(&self.device, format, size)?;
                textures.push(PoolTexture {
                    id: next_id(),
                    texture,
                    handle: Arc::new(handle),
                    released: Arc::default(),
                });
            }
            self.pool = Some(Pool {
                id: next_id(),
                format,
                size,
                textures,
                leases: SlotLeases::new(self.pool_size),
                next: 0,
            });
        }
        self.pool.as_ref().ok_or_else(|| "no pool".to_owned())
    }

    /// Copies `picture` (a slice of the decoder's array, on this device)
    /// into a free pool texture and returns it as a [`SharedPicture`]-backed
    /// picture; `None` when every pool texture is still held (the picture is
    /// dropped and counted).
    ///
    /// # Errors
    ///
    /// Pool creation, a reader's fence that cannot be opened, or the signal.
    pub fn export(&mut self, picture: &D3d11Picture) -> Result<Option<D3d11Picture>, String> {
        let started = Instant::now();
        let pool_size = usize::from(self.pool_size);
        let free = {
            let pool = self.pool_for(picture.format, picture.coded_size)?;
            (0..pool_size)
                .map(|offset| (pool.next + offset) % pool_size)
                .find(|index| u8::try_from(*index).is_ok_and(|slot| !pool.leases.held(Slot(slot))))
        };
        let Some(index) = free else {
            self.timing.exhausted += 1;
            self.log_if_due(picture.coded_size);
            return Ok(None);
        };
        let (texture, handle, texture_id, pool_id, released, leases) = {
            let pool = self.pool.as_mut().ok_or("no pool")?;
            pool.next = (index + 1) % pool_size;
            let entry = &pool.textures[index];
            (
                entry.texture.clone(),
                Arc::clone(&entry.handle),
                entry.id,
                pool.id,
                Arc::clone(&entry.released),
                Arc::clone(&pool.leases),
            )
        };
        let readers = released.take();
        let mut waits = Vec::with_capacity(readers.len());
        for (fence, value) in readers {
            if self.readers.len() > 32 && !self.readers.contains_key(&fence.id) {
                self.readers.clear();
            }
            let opened = if let Some(opened) = self.readers.get(&fence.id) {
                opened.clone()
            } else {
                let opened = fence.open(&self.device)?;
                self.readers.insert(fence.id, opened.clone());
                opened
            };
            waits.push((opened, value));
        }
        {
            let _context = lock(&self.context_lock);
            // SAFETY: live interfaces on this device; the copy is a whole
            // subresource of identical size and format.
            unsafe {
                for (fence, value) in &waits {
                    // Render → decode: every reader's draws are done first.
                    self.context
                        .Wait(fence, *value)
                        .map_err(|error| format!("Wait(presenter fence): {error}"))?;
                }
                self.context.CopySubresourceRegion(
                    &texture,
                    0,
                    0,
                    0,
                    0,
                    &picture.texture,
                    picture.array_slice,
                    None,
                );
                self.ready_value += 1;
                // Decode → render.
                self.context
                    .Signal(&self.ready, self.ready_value)
                    .map_err(|error| format!("Signal(decode fence): {error}"))?;
                // Submit now: a presenter on another device waits for it.
                self.context.Flush();
            }
        }
        self.timing.exports += 1;
        self.timing.reader_waits += u32::try_from(waits.len()).unwrap_or(u32::MAX);
        let took = started.elapsed();
        self.timing.total += took;
        self.timing.total_max = self.timing.total_max.max(took);
        self.log_if_due(picture.coded_size);
        let slot = Slot(u8::try_from(index).unwrap_or(u8::MAX));
        Ok(Some(D3d11Picture {
            texture,
            array_slice: 0,
            format: picture.format,
            coded_size: picture.coded_size,
            visible: picture.visible,
            fence: Some(self.ready.clone()),
            fence_value: self.ready_value,
            shader_resource: true,
            lease: SlotLease::new(&leases, slot) as Arc<dyn std::any::Any + Send + Sync>,
            shared: Some(SharedPicture {
                texture_id,
                pool_id,
                texture: handle,
                ready: self.ready_shared.clone(),
                ready_value: self.ready_value,
                released,
            }),
        }))
    }

    fn log_if_due(&mut self, size: (u32, u32)) {
        if self.timing.since.elapsed() < Duration::from_secs(5) {
            return;
        }
        let timing = &self.timing;
        let us = |value: Duration| u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        tracing::info!(
            target: arcen_deck_core::log_target::VIDEO,
            decoder = self.decoder,
            size = ?size,
            exports = timing.exports,
            export_avg_us = us(timing.total / timing.exports.max(1)),
            export_max_us = us(timing.total_max),
            reader_waits = timing.reader_waits,
            pool = self.pool_size,
            pool_exhausted = timing.exhausted,
            "decoded picture hand-over timing"
        );
        self.timing = ExportTiming::new();
    }
}

// SAFETY: the device is free-threaded and multithread-protected; the
// export is used by one thread at a time (the media worker's decoder).
unsafe impl Send for SharedExport {}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test fails loudly on a missing device or a failed call"
)]
mod tests {
    use super::*;
    use crate::decode::backend::SHARED_POOL;
    use crate::decode::device::{driver_version, hardware_adapters};
    use arcen_deck_core::decode::VideoDecodeError;

    fn first_unit(keyframe: &crate::decode::probe::ProbeKeyframe) -> &'static [u8] {
        let nal = match keyframe.codec {
            StreamCodecKind::H264 => NalCodec::H264,
            StreamCodecKind::H265 => NalCodec::H265,
        };
        split_access_units(nal, keyframe.stream)
            .into_iter()
            .next()
            .expect("an access unit")
    }

    /// A picture decoded on the decoder's own device reads back sample-exact
    /// on another device through its NT handle and the decode fence, in
    /// every format the proof shares; a format it does not share is refused,
    /// never handed over; the pool is bounded (a full pool drops the newest
    /// picture instead of waiting) and a texture released with a
    /// presenter's fence is written again. Runs on a Windows machine with a
    /// qualified hardware decoder (the Windows lab); elsewhere it says why
    /// it skipped.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario per profile: decode, read on both devices, release, reuse"
    )]
    fn a_picture_decoded_on_its_own_device_reads_exactly_on_another() {
        let Some((adapter, name, vendor, device_id, _)) = hardware_adapters()
            .ok()
            .and_then(|adapters| adapters.into_iter().next())
        else {
            eprintln!("skipped: no hardware adapter");
            return;
        };
        let Ok(mut decode) = VideoDevice::create(&adapter, name.clone(), vendor, device_id) else {
            eprintln!("skipped: no video device on {name}");
            return;
        };
        decode.driver_version = driver_version(&adapter);
        let presenter =
            VideoDevice::create(&adapter, name, vendor, device_id).expect("a second device");
        let proof = match prove_sharing_now(&decode, &presenter.device, &presenter.context_lock) {
            Ok(proof) => proof,
            Err(reason) => {
                eprintln!("skipped: {reason}");
                return;
            }
        };
        eprintln!("{}", proof.describe());
        let presenter_context: ID3D11DeviceContext4 =
            unsafe { presenter.device.GetImmediateContext() }
                .and_then(|context| context.cast())
                .expect("ID3D11DeviceContext4");
        let mut proved = 0;
        for keyframe in &PROBE_KEYFRAMES {
            if decode.probe(&keyframe.profile).is_err() {
                continue;
            }
            let unit = first_unit(keyframe);
            if !proof.formats.contains(&keyframe.profile.output) {
                let mut limited = D3d11VideoDecoder::exporting(
                    decode.clone(),
                    Some(proof.formats.clone().into()),
                );
                assert!(
                    matches!(
                        limited.decode_annex_b(keyframe.codec, unit),
                        Err(VideoDecodeError::Unsupported(_))
                    ),
                    "{} is not shared, so it is refused",
                    keyframe.profile.name
                );
                continue;
            }
            let mut decoder = D3d11VideoDecoder::exporting(decode.clone(), None);
            let mut held = Vec::new();
            for _ in 0..SHARED_POOL {
                let mut pictures = decoder
                    .decode_annex_b(keyframe.codec, unit)
                    .expect("decode");
                held.push(pictures.pop().expect("a picture"));
            }
            assert!(
                decoder
                    .decode_annex_b(keyframe.codec, unit)
                    .expect("decode")
                    .is_empty(),
                "a full pool drops the newest picture"
            );
            let picture = held.pop().expect("a held picture");
            let shared = picture.shared.clone().expect("a shared picture");
            let texture = open_texture(&presenter.device, &shared.texture).expect("open texture");
            let ready = shared.ready.open(&presenter.device).expect("open fence");
            unsafe { presenter_context.Wait(&ready, shared.ready_value) }.expect("Wait");
            let local = D3d11Picture {
                texture,
                fence: None,
                shared: None,
                ..picture.clone()
            };
            let other = fnv1a(
                &read_planar(&presenter.device, &presenter.context_lock, &local)
                    .expect("read back on the other device"),
            );
            assert_eq!(
                other, keyframe.fnv,
                "{} differs on the other device",
                keyframe.profile.name
            );
            let (release, release_shared) =
                SharedFence::create(&presenter.device).expect("release fence");
            unsafe {
                presenter_context.Signal(&release, 1).expect("Signal");
                presenter_context.Flush();
            }
            shared.released.record(&release_shared, 1);
            drop((local, picture));
            let again = decoder
                .decode_annex_b(keyframe.codec, unit)
                .expect("decode again");
            assert_eq!(
                again
                    .first()
                    .and_then(|picture| picture.shared.as_ref())
                    .map(|shared| shared.texture_id),
                Some(shared.texture_id),
                "the released texture is written again"
            );
            proved += 1;
        }
        assert!(proved > 0, "no format is shared on a qualified adapter");
    }
}
