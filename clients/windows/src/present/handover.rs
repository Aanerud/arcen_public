// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The presenter's side of the decoded-picture hand-over
//! (`crate::decode::shared`): a picture decoded on the decoder's own device
//! is opened on this presenter's device through its NT handle, sampled
//! only after a GPU-side wait for the decoder's fence, and released by
//! signalling this presenter's own shared fence after the draws that read
//! it; the decoder waits GPU-side for that fence before it writes the
//! texture again. A picture decoded on this presenter's device (the
//! decoder shares it) is sampled in place after its decode fence, as
//! before.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D};
use windows::core::Interface;

use super::device::Gpu;
use super::picture::D3d11Picture;
use super::{Result, hr};
use crate::decode::shared::{SharedFence, open_texture};

#[derive(Default)]
pub struct SharedReads {
    /// Opened textures of the current pool only (by texture id): an open
    /// texture keeps the shared resource alive, so a retired pool's are
    /// closed as soon as a picture of a newer pool arrives.
    textures: HashMap<u64, (u64, ID3D11Texture2D)>,
    /// Opened decode fences, the current decoder's only (by fence id).
    fences: HashMap<u64, ID3D11Fence>,
    /// This presenter's release fence (render → decode).
    release: Option<(ID3D11Fence, SharedFence)>,
    release_value: u64,
    /// Pictures whose decode had not finished on the GPU when this
    /// presenter queued its draw (the GPU waited for it).
    decode_pending: u64,
}

fn context4(gpu: &Gpu) -> Result<ID3D11DeviceContext4> {
    hr("ID3D11DeviceContext4", gpu.context.cast())
}

impl SharedReads {
    /// The picture to sample on `gpu`, with the GPU-side wait for its
    /// decode queued on `gpu`'s context. Call [`Self::release`] after the
    /// draws that read it.
    pub fn acquire(&mut self, gpu: &Gpu, picture: &D3d11Picture) -> Result<D3d11Picture> {
        let Some(shared) = &picture.shared else {
            if let Some(fence) = &picture.fence {
                // The decoder's work, GPU-side, before anything samples it.
                hr("Wait(decode fence)", unsafe {
                    context4(gpu)?.Wait(fence, picture.fence_value)
                })?;
            }
            return Ok(picture.clone());
        };
        let texture = if let Some((_, texture)) = self.textures.get(&shared.texture_id) {
            texture.clone()
        } else {
            let texture = open_texture(&gpu.device, &shared.texture)?;
            self.textures.retain(|_, (pool, _)| *pool == shared.pool_id);
            self.textures
                .insert(shared.texture_id, (shared.pool_id, texture.clone()));
            texture
        };
        let ready = if let Some(fence) = self.fences.get(&shared.ready.id) {
            fence.clone()
        } else {
            let fence = shared.ready.open(&gpu.device)?;
            self.fences.clear();
            self.fences.insert(shared.ready.id, fence.clone());
            fence
        };
        if unsafe { ready.GetCompletedValue() } < shared.ready_value {
            self.decode_pending += 1;
        }
        // Decode → render.
        hr("Wait(shared decode fence)", unsafe {
            context4(gpu)?.Wait(&ready, shared.ready_value)
        })?;
        Ok(D3d11Picture {
            texture,
            fence: None,
            fence_value: 0,
            shared: None,
            ..picture.clone()
        })
    }

    /// The draws that read `picture` are queued: signal this presenter's
    /// release fence after them and tell the decoder (render → decode).
    pub fn release(&mut self, gpu: &Gpu, picture: &D3d11Picture) -> Result<()> {
        let Some(shared) = &picture.shared else {
            return Ok(());
        };
        if self.release.is_none() {
            self.release = Some(SharedFence::create(&gpu.device)?);
        }
        let Some((fence, handle)) = &self.release else {
            return Err("no release fence".into());
        };
        self.release_value += 1;
        hr("Signal(release fence)", unsafe {
            context4(gpu)?.Signal(fence, self.release_value)
        })?;
        shared.released.record(handle, self.release_value);
        // Submitted now, so the decoder never waits on a signal that is
        // still only recorded on this context.
        unsafe { gpu.context.Flush() };
        Ok(())
    }

    /// Teardown: waits (bounded) until the GPU passed this presenter's last
    /// release, so no decoder waits on a fence of a device that is going
    /// away.
    pub fn finish(&self, timeout: Duration) {
        let Some((fence, _)) = &self.release else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while unsafe { fence.GetCompletedValue() } < self.release_value && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Pictures whose decode was still running when they were drawn, since
    /// the last call.
    pub fn take_decode_pending(&mut self) -> u64 {
        std::mem::take(&mut self.decode_pending)
    }
}

super::opaque_debug!(SharedReads);
