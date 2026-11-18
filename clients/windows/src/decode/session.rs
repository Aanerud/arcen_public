// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One `ID3D11VideoDecoder` with its output texture array, and the
//! submission of one picture's DXVA buffers.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use arcen_media::bitstream::picture::{Slot, StreamFormat};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_DECODER, D3D11_BIND_SHADER_RESOURCE, D3D11_FENCE_FLAG_NONE, D3D11_TEX2D_VDOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VDOV_DIMENSION_TEXTURE2D,
    D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, D3D11_VIDEO_DECODER_BUFFER_DESC,
    D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
    D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
    D3D11_VIDEO_DECODER_BUFFER_TYPE, D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC_0, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence,
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDecoder, ID3D11VideoDecoderOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use super::device::{DecoderProfile, VideoDevice};

/// `E_PENDING`: the decoder is still busy with an earlier frame.
const E_PENDING: windows::core::HRESULT = windows::core::HRESULT(0x8000_000A_u32.cast_signed());

/// Per-slot counts of decoded pictures handed out and still alive.
#[derive(Debug, Default)]
pub struct SlotLeases {
    counts: Vec<AtomicU32>,
}

impl SlotLeases {
    pub(crate) fn new(size: u8) -> Arc<Self> {
        Arc::new(Self {
            counts: (0..size).map(|_| AtomicU32::new(0)).collect(),
        })
    }

    /// Whether a handed-out picture still holds `slot`.
    #[must_use]
    pub fn held(&self, slot: Slot) -> bool {
        self.counts
            .get(slot.index())
            .is_some_and(|count| count.load(Ordering::Acquire) > 0)
    }
}

/// Keeps one slot from being decoded into while a picture referring to it
/// lives; shared by every clone of that picture.
#[derive(Debug)]
pub struct SlotLease {
    leases: Arc<SlotLeases>,
    slot: Slot,
}

impl SlotLease {
    pub(crate) fn new(leases: &Arc<SlotLeases>, slot: Slot) -> Arc<Self> {
        if let Some(count) = leases.counts.get(slot.index()) {
            count.fetch_add(1, Ordering::AcqRel);
        }
        Arc::new(Self {
            leases: Arc::clone(leases),
            slot,
        })
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if let Some(count) = self.leases.counts.get(self.slot.index()) {
            count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// The decoder → presenter picture (the Windows
/// `VideoDecoderBackend::NativeFrame`): one slice of a texture on the
/// decoder's device.
///
/// When the decoder runs on the presenter's device, sample `texture` only
/// after waiting GPU-side on `fence` (when present) for `fence_value` (the
/// readiness rule). When it runs on a device of its own, `shared` says how
/// a presenter opens the picture on its device and orders its reads
/// against the decoder (`super::shared`). Either way the texture is not
/// written again while any clone of this value lives (the slot-lifetime
/// rule).
#[derive(Clone)]
pub struct D3d11Picture {
    /// NV12, P010, Y410 or AYUV.
    pub texture: ID3D11Texture2D,
    pub array_slice: u32,
    pub format: DXGI_FORMAT,
    /// The texture's size (the coded size, aligned up for the driver).
    pub coded_size: (u32, u32),
    /// The picture to show inside it: left, top, width, height.
    pub visible: (u32, u32, u32, u32),
    /// Waited GPU-side for `fence_value` before sampling.
    pub fence: Option<ID3D11Fence>,
    pub fence_value: u64,
    /// Whether the texture can be bound as a shader resource directly; if
    /// not, the presenter copies the visible picture out first.
    pub shader_resource: bool,
    /// Keeps the decoder's slot alive (a [`SlotLease`] for decoded
    /// pictures).
    pub lease: Arc<dyn Any + Send + Sync>,
    /// Set when the texture lives on the decoder's own device: the NT
    /// handles and fences a presenter on another device reads it through.
    pub shared: Option<super::shared::SharedPicture>,
}

impl D3d11Picture {
    /// A whole, shader-bindable texture with nothing to wait for (tests,
    /// synthetic frames).
    #[must_use]
    pub fn texture(texture: ID3D11Texture2D) -> Self {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: reads the description of a live texture.
        #[allow(unsafe_code)]
        unsafe {
            texture.GetDesc(&mut desc);
        }
        Self {
            texture,
            array_slice: 0,
            format: desc.Format,
            coded_size: (desc.Width, desc.Height),
            visible: (0, 0, desc.Width, desc.Height),
            fence: None,
            fence_value: 0,
            shader_resource: true,
            lease: Arc::new(()),
            shared: None,
        }
    }

    /// The visible size: what is presented and what Exact still image grids
    /// are compared with.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.visible.2, self.visible.3)
    }

    /// Whether the planes can be sampled in place: bindable, and the
    /// visible picture starts at the surface's origin (a bottom/right crop
    /// only shortens what the shader reads).
    #[must_use]
    pub const fn samples_in_place(&self) -> bool {
        self.shader_resource && self.visible.0 == 0 && self.visible.1 == 0
    }
}

impl std::fmt::Debug for D3d11Picture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("D3d11Picture")
            .field("array_slice", &self.array_slice)
            .field("format", &self.format)
            .field("coded_size", &self.coded_size)
            .field("visible", &self.visible)
            .field("fence_value", &self.fence_value)
            .field("shader_resource", &self.shader_resource)
            .field("shared", &self.shared.is_some())
            .finish_non_exhaustive()
    }
}

// SAFETY: the texture and fence belong to the shared, multithread-protected
// device; multi-call sequences hold the device's context lock, and the
// lease keeps the slot from being reused.
#[allow(unsafe_code)]
unsafe impl Send for D3d11Picture {}
// SAFETY: as above; the presenter only reads through shared references.
#[allow(unsafe_code)]
unsafe impl Sync for D3d11Picture {}

/// The DXVA buffers of one picture.
#[derive(Debug, Default)]
pub struct PictureBuffers<'a> {
    pub picture_parameters: &'a [u8],
    pub inverse_quantization: Option<&'a [u8]>,
    pub slice_control: &'a [u8],
    pub slice_count: u32,
    /// `NumMBsInBuffer`: the picture's macroblocks for H.264, 0 for HEVC.
    pub macroblocks: u32,
    pub bitstream: &'a [u8],
}

/// A decoder object for one stream format.
#[derive(Debug)]
pub struct DecoderSession {
    pub profile: DecoderProfile,
    pub format: StreamFormat,
    decoder: ID3D11VideoDecoder,
    context: ID3D11VideoContext,
    context_lock: Arc<std::sync::Mutex<()>>,
    texture: ID3D11Texture2D,
    surface_size: (u32, u32),
    views: Vec<ID3D11VideoDecoderOutputView>,
    shader_resource: bool,
    fence: Option<(ID3D11Fence, ID3D11DeviceContext4)>,
    fence_value: u64,
    pub leases: Arc<SlotLeases>,
    timing: DecodeTiming,
}

/// Where a decode call's time goes, logged every five seconds: waiting for
/// the shared context lock, retrying while the decoder is busy
/// (`E_PENDING`), and submitting the picture. A decoder slower than the
/// stream shows here first.
#[derive(Debug)]
struct DecodeTiming {
    /// Which decoder (one per monitor stream) this is.
    decoder: u64,
    since: std::time::Instant,
    decodes: u32,
    pending_retries: u32,
    lock_wait: std::time::Duration,
    lock_wait_max: std::time::Duration,
    total: std::time::Duration,
    total_max: std::time::Duration,
}

impl DecodeTiming {
    fn new(decoder: u64) -> Self {
        Self {
            decoder,
            since: std::time::Instant::now(),
            decodes: 0,
            pending_retries: 0,
            lock_wait: std::time::Duration::ZERO,
            lock_wait_max: std::time::Duration::ZERO,
            total: std::time::Duration::ZERO,
            total_max: std::time::Duration::ZERO,
        }
    }

    const EVERY: std::time::Duration = std::time::Duration::from_secs(5);

    fn note(
        &mut self,
        lock_wait: std::time::Duration,
        retries: u32,
        total: std::time::Duration,
        size: (u32, u32),
    ) {
        self.decodes += 1;
        self.pending_retries += retries;
        self.lock_wait += lock_wait;
        self.lock_wait_max = self.lock_wait_max.max(lock_wait);
        self.total += total;
        self.total_max = self.total_max.max(total);
        if self.since.elapsed() < Self::EVERY {
            return;
        }
        let avg_us = |sum: std::time::Duration| {
            u64::try_from(sum.as_micros() / u128::from(self.decodes.max(1))).unwrap_or(u64::MAX)
        };
        let us = |value: std::time::Duration| u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        tracing::info!(
            target: arcen_deck_core::log_target::VIDEO,
            decoder = self.decoder,
            size = ?size,
            decodes = self.decodes,
            pending_retries = self.pending_retries,
            lock_wait_avg_us = avg_us(self.lock_wait),
            lock_wait_max_us = us(self.lock_wait_max),
            decode_call_avg_us = avg_us(self.total),
            decode_call_max_us = us(self.total_max),
            "decoder timing"
        );
        *self = Self::new(self.decoder);
    }
}

impl DecoderSession {
    /// Creates the decoder and `pool` output surfaces for `format`.
    ///
    /// # Errors
    ///
    /// A reason, for the refusal message or the log.
    pub fn create(
        device: &VideoDevice,
        profile: DecoderProfile,
        format: StreamFormat,
        pool: u8,
        decoder_number: u64,
    ) -> Result<Self, String> {
        let (desc, config) =
            device.configuration(&profile, format.coded_width, format.coded_height)?;
        let pool = pool.max(u8::try_from(config.ConfigMinRenderTargetBuffCount).unwrap_or(0));
        // DXVA HEVC asks for 128-pixel aligned surfaces (H.264: macroblocks).
        let alignment = match profile.codec {
            arcen_media::bitstream::picture::StreamCodecKind::H265 => 128,
            arcen_media::bitstream::picture::StreamCodecKind::H264 => 16,
        };
        let surface_width = format.coded_width.next_multiple_of(alignment);
        let surface_height = format.coded_height.next_multiple_of(alignment);
        // SAFETY: COM object creation with owned out-parameters; the
        // descriptors outlive the calls.
        #[allow(unsafe_code)]
        unsafe {
            let context: ID3D11VideoContext = device
                .device
                .GetImmediateContext()
                .and_then(|context| context.cast())
                .map_err(|error| format!("no video context: {error}"))?;
            let decoder = device
                .video
                .CreateVideoDecoder(&desc, &config)
                .map_err(|error| format!("{} decoder creation failed: {error}", profile.name))?;
            let mut texture_desc = D3D11_TEXTURE2D_DESC {
                Width: surface_width,
                Height: surface_height,
                MipLevels: 1,
                ArraySize: u32::from(pool),
                Format: profile.output,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: u32::try_from(D3D11_BIND_DECODER.0 | D3D11_BIND_SHADER_RESOURCE.0)
                    .unwrap_or(0),
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture = None;
            let mut shader_resource = true;
            if device
                .device
                .CreateTexture2D(&texture_desc, None, Some(&mut texture))
                .is_err()
            {
                // Some drivers refuse sampling decoder arrays; the presenter
                // then copies the slice.
                shader_resource = false;
                texture_desc.BindFlags = u32::try_from(D3D11_BIND_DECODER.0).unwrap_or(0);
                device
                    .device
                    .CreateTexture2D(&texture_desc, None, Some(&mut texture))
                    .map_err(|error| format!("decoder surfaces: {error}"))?;
            }
            let texture = texture.ok_or("decoder surfaces: none returned")?;
            let mut views = Vec::with_capacity(usize::from(pool));
            for slice in 0..u32::from(pool) {
                let view_desc = D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC {
                    DecodeProfile: profile.guid,
                    ViewDimension: D3D11_VDOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VDOV { ArraySlice: slice },
                    },
                };
                let mut view = None;
                device
                    .video
                    .CreateVideoDecoderOutputView(&texture, &view_desc, Some(&mut view))
                    .map_err(|error| format!("decoder output view {slice}: {error}"))?;
                views.push(view.ok_or("decoder output view: none returned")?);
            }
            let fence = device
                .device
                .cast::<ID3D11Device5>()
                .ok()
                .and_then(|device5| {
                    let mut fence: Option<ID3D11Fence> = None;
                    device5
                        .CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)
                        .ok()?;
                    let context4: ID3D11DeviceContext4 =
                        device.device.GetImmediateContext().ok()?.cast().ok()?;
                    Some((fence?, context4))
                });
            Ok(Self {
                profile,
                format,
                decoder,
                context,
                context_lock: Arc::clone(&device.context_lock),
                texture,
                surface_size: (surface_width, surface_height),
                views,
                shader_resource,
                fence,
                fence_value: 0,
                leases: SlotLeases::new(pool),
                timing: DecodeTiming::new(decoder_number),
            })
        }
    }

    /// Surfaces in the pool.
    #[must_use]
    pub fn pool_size(&self) -> u8 {
        u8::try_from(self.views.len()).unwrap_or(u8::MAX)
    }

    /// Decodes one picture into `slot`.
    ///
    /// # Errors
    ///
    /// The failing call, for the log; the caller resets and asks for a
    /// keyframe.
    pub fn decode(&mut self, slot: Slot, buffers: &PictureBuffers<'_>) -> Result<(), String> {
        let view = self
            .views
            .get(slot.index())
            .ok_or("slot outside the decoder pool")?
            .clone();
        // The immediate context is shared with the presenters and the cursor
        // layer: the whole begin → buffers → end → signal sequence is one.
        // While the decoder is busy (E_PENDING) the lock is released, so a
        // waiting decode never stalls presentation.
        let lock = Arc::clone(&self.context_lock);
        let mut attempts = 0;
        let started = std::time::Instant::now();
        let mut lock_wait = std::time::Duration::ZERO;
        // SAFETY: the decoder, context and view are live COM objects; every
        // buffer pointer comes from GetDecoderBuffer with its reported size
        // and is released before submission.
        #[allow(unsafe_code)]
        unsafe {
            let _guard = loop {
                let asked = std::time::Instant::now();
                let guard = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                lock_wait += asked.elapsed();
                match self
                    .context
                    .DecoderBeginFrame(&self.decoder, &view, 0, None)
                {
                    Ok(()) => break guard,
                    Err(error) if error.code() == E_PENDING && attempts < 200 => {
                        drop(guard);
                        attempts += 1;
                        std::thread::sleep(std::time::Duration::from_micros(500));
                    }
                    Err(error) => return Err(format!("DecoderBeginFrame: {error}")),
                }
            };
            let result = self.submit(buffers);
            let end = self.context.DecoderEndFrame(&self.decoder);
            result?;
            end.map_err(|error| format!("DecoderEndFrame: {error}"))?;
            if let Some((fence, context)) = &self.fence {
                self.fence_value += 1;
                context
                    .Signal(fence, self.fence_value)
                    .map_err(|error| format!("fence signal: {error}"))?;
            }
        }
        let size = self.surface_size;
        self.timing
            .note(lock_wait, attempts, started.elapsed(), size);
        Ok(())
    }

    #[allow(unsafe_code)]
    unsafe fn submit(&self, buffers: &PictureBuffers<'_>) -> Result<(), String> {
        let mut descs = Vec::with_capacity(4);
        let mut put = |kind: D3D11_VIDEO_DECODER_BUFFER_TYPE,
                       data: &[u8],
                       count: u32|
         -> Result<(), String> {
            let mut size = 0_u32;
            let mut pointer = std::ptr::null_mut();
            // SAFETY: see `decode`.
            unsafe {
                self.context
                    .GetDecoderBuffer(&self.decoder, kind, &mut size, &mut pointer)
                    .map_err(|error| format!("GetDecoderBuffer {kind:?}: {error}"))?;
                if pointer.is_null() || (size as usize) < data.len() {
                    let _ = self.context.ReleaseDecoderBuffer(&self.decoder, kind);
                    return Err(format!(
                        "decoder buffer {kind:?} holds {size} bytes, {} needed",
                        data.len()
                    ));
                }
                std::ptr::copy_nonoverlapping(data.as_ptr(), pointer.cast::<u8>(), data.len());
                self.context
                    .ReleaseDecoderBuffer(&self.decoder, kind)
                    .map_err(|error| format!("ReleaseDecoderBuffer {kind:?}: {error}"))?;
            }
            descs.push(D3D11_VIDEO_DECODER_BUFFER_DESC {
                BufferType: kind,
                DataSize: u32::try_from(data.len()).unwrap_or(u32::MAX),
                NumMBsInBuffer: count,
                ..Default::default()
            });
            Ok(())
        };
        put(
            D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS,
            buffers.picture_parameters,
            0,
        )?;
        if let Some(matrix) = buffers.inverse_quantization {
            put(
                D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
                matrix,
                0,
            )?;
        }
        put(
            D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
            buffers.slice_control,
            buffers.macroblocks,
        )?;
        put(
            D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
            buffers.bitstream,
            buffers.macroblocks,
        )?;
        // SAFETY: the descriptors describe the buffers released above.
        unsafe {
            self.context
                .SubmitDecoderBuffers(&self.decoder, &descs)
                .map_err(|error| format!("SubmitDecoderBuffers: {error}"))
        }
    }

    /// The picture in `slot`, leased until its last clone is dropped.
    #[must_use]
    pub fn picture(&self, slot: Slot) -> D3d11Picture {
        let (left, top, width, height) = self.format.visible;
        D3d11Picture {
            texture: self.texture.clone(),
            array_slice: u32::from(slot.0),
            format: self.profile.output,
            coded_size: self.surface_size,
            visible: (left, top, width, height),
            fence: self.fence.as_ref().map(|(fence, _)| fence.clone()),
            fence_value: self.fence_value,
            shader_resource: self.shader_resource,
            lease: SlotLease::new(&self.leases, slot) as Arc<dyn Any + Send + Sync>,
            shared: None,
        }
    }
}
