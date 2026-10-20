//! Wayland provider capture: bounded `PipeWire` buffer leases and GPU import.
//!
//! The Linux Pier's Wayland session adapter owns the compositor and hands
//! capenc a `PipeWire` node ([`handoff`]). This module consumes that stream
//! and delivers validated frames to an import route; it does not choose a
//! preset. Each preset pipeline composes, once per epoch:
//!
//! 1. a source: `pipewire::PipeWireSource::start` with a [`format::FormatOffer`] the
//!    pipeline chose (the eight-bit SDR family for Auto/Speed);
//! 2. an import route from [`evidence::decide_import`]:
//!    `host_copy::HostCopyImporter` (`host-copy`) or, on a host whose
//!    qualification verified it, `gpu_convert::GpuConvertImporter`
//!    (`gpu-convert`: DMA-BUF -> EGL image -> GL blit into a CUDA-registered
//!    texture -> CUDA copy; same GPU only);
//! 3. the existing CUDA/NVENC encoder (`nvenc_cuda::Encoder`), staged from
//!    the retained frame either route writes; and
//! 4. admission input from [`evidence::capture_source_evidence`].
//!
//! No preset or mode flag reaches the `PipeWire` callback: it only dequeues,
//! classifies ([`layout`]), validates (`CaptureSurface::validate`) and pushes
//! a lease into a bounded `arcen_media::video::CaptureMailbox`. Superseded frames are
//! requeued at once and their damage is carried forward; a lease returns its
//! buffer to `PipeWire` only when the import worker drops it, after the copy
//! that read it has completed.
//!
//! ## DMA-BUF synchronisation and release
//!
//! With DMA-BUF offered, capenc asks for `PipeWire` explicit sync
//! (`SPA_META_SyncTimeline`, [`explicit_sync`]); Mutter 49 grants it on a DRM
//! device with timeline syncobjs. The import worker (never the callback)
//! waits for the buffer's acquire point before the blit reads it, waits for
//! the blit's GL fence, and only then drops the lease; the loop thread
//! signals the release point immediately before queueing the buffer back.
//! A DMA-BUF without sync points is synchronised implicitly: the worker
//! waits for the buffer's exported read fence
//! ([`dmabuf::wait_for_producer`]) and refuses the import when the kernel
//! cannot export one. An import failure releases everything it created,
//! encodes nothing from that frame, and withdraws DMA-BUF from the offer so
//! the producer renegotiates to shared memory (an explicit, logged fallback).

#![cfg_attr(not(feature = "wayland"), allow(dead_code))]

pub mod cursor;
pub mod dmabuf;
pub mod evidence;
pub mod explicit_sync;
pub mod format;
pub mod gpu;
pub mod handoff;
pub mod layout;
pub mod pipelines;
pub mod reference;

#[cfg(all(feature = "wayland", target_os = "linux"))]
pub mod gpu_convert;
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub mod host_copy;
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub mod materialise;
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub mod pipewire;
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub mod probe;
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub mod ramp_client;
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub mod telemetry;
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub mod wide_readback;
