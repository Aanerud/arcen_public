// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows Deck's video presenter: the D3D11 / DirectComposition twin
//! of the Mac's dedicated Metal layers (`clients/macos/src/ui/
//! video_metal_layer.rs`, `refine_overlay.rs`).
//!
//! ```text
//! HWND (native menu bar; GDI fills the letterbox under the tree)
//!  └─ IDCompositionTarget (Deck-owned, topmost)   composition.rs
//!      └─ root
//!          ├─ video visual   D3D11 flip swapchain  swapchain.rs  ← bottom
//!          ├─ cursor visual  host cursor / pen dot composition.rs
//!          └─ overlay visual egui (wgpu DX12, the `ui` stream's shell)   ← top
//!                            clipped to what egui drew, out of the tree when nothing
//! ```
//!
//! - **Four contracts, separate pipelines** (root `AGENTS.md`): Auto/Speed
//!   present `B8G8R8A8` tagged `G22_P709`; Grading `R10G10B10A2` tagged
//!   `G22_P709`, no HDR metadata; HDR `R10G10B10A2` tagged `G2084_P2020`
//!   with HDR10 metadata, **only when the host returned PQ**
//!   ([`arcen_media::video::present::presents_pq`]); FP16 scRGB is the
//!   fallback for a driver whose PQ composition misbehaves.
//! - **One conversion** (`shaders/present.hlsl`) reading the shared
//!   [`arcen_media::video::present::VideoUniform`], compiled to DXBC at
//!   build time; GPU output is tested against the shared CPU references.
//! - **Exact still image** ([`compose::FrameComposer`]): overlay writes on
//!   the presenter's own immediate context before the draw that samples
//!   them, presented fences after an event query, level-2/3 readbacks; all
//!   decisions are `arcen_media::refine`'s.
//! - **One adapter, a device per window**: the one that owns the root
//!   window's monitor ([`device::AdapterChoice::Monitor`]); each further
//!   window presents on a device of its own on the same adapter, and the
//!   decoder decodes on its own device and hands pictures over through
//!   shared textures and fences ([`handover`]), so one window's `Present`
//!   never holds up decode or another window. Where the driver cannot share
//!   them, everything shares the root window's device, as before.
//! - **The canvas** ([`canvas`], ADR 0028): the eight-bit sink keeps one
//!   persistent `B8G8R8A8` canvas per monitor on its device, applies each
//!   canvas record to it in order on the immediate context (copies, fills,
//!   tiles, cache, surface paints through the same conversion pass) and
//!   presents it by copying it into the swapchain; its oracle is the shared
//!   CPU executor `arcen_media::canvas::CanvasPixels`.
//! - **Ten bits at the glass are measured, not assumed** ([`depth`]).
//! - **DWM composes unless the video alone covers the monitor** ([`flip`]):
//!   fullscreen, 1:1, opaque, nothing drawn over it (the connection bar
//!   appears only on demand).
//! - **When a frame is presented is the pipeline's shared
//!   `PresentTiming`**: Auto and Speed at once without tearing (sync
//!   interval 0), Grading and HDR on the refresh (sync interval 1).

#![allow(
    unsafe_code,
    reason = "D3D11, DXGI and DirectComposition are COM APIs; every call is an FFI call"
)]
#![allow(
    clippy::borrow_as_ptr,
    clippy::wildcard_imports,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::doc_markdown,
    reason = "COM out-parameters, `windows` namespaces and D3D's u32/i32/f32 sizes and \
              coordinates; errors are named in every message"
)]

pub mod canvas;
pub mod compose;
pub mod composition;
pub mod convert;
pub mod depth;
pub mod device;
pub mod display;
pub mod flip;
pub mod handover;
#[cfg(any(test, arcen_present_hub_e2e))]
pub mod hub_e2e;
pub mod picture;
pub mod refine;
pub mod selftest;
pub mod sink;
pub mod swapchain;
pub mod thread;

pub use compose::{DecodedFrame, FrameComposer, RefineFrameTag};
pub use device::{AdapterChoice, Gpu};
pub use picture::D3d11Picture;
pub use sink::D3d11VideoSink;
pub use swapchain::{PresentMode, VideoSwapchain};

/// A presenter failure, always with the reason.
pub type Result<T> = std::result::Result<T, String>;

/// Maps a COM result to a [`Result`] naming the call.
pub(crate) fn hr<T>(what: &str, result: windows::core::Result<T>) -> Result<T> {
    result.map_err(|error| format!("{what}: {error}"))
}

/// The object a successful COM `Create*` call wrote to its out-parameter.
pub(crate) fn created<T>(what: &str, object: Option<T>) -> Result<T> {
    object.ok_or_else(|| format!("{what}: no object returned"))
}

/// `Debug` for types that hold COM objects: the name only.
macro_rules! opaque_debug {
    ($($name:ident),+ $(,)?) => {
        $(impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        })+
    };
}
pub(crate) use opaque_debug;
