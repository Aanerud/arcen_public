// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Video decode.
//!
//! Arcen's own D3D11 video decoder: `ID3D11VideoDecoder` filled from DXVA
//! picture parameters for H.264 and HEVC, including HEVC `RExt` 4:4:4 10-bit
//! (`HEVC_VLD_MAIN10_444` → `DXGI_FORMAT_Y410`), implementing
//! `arcen_deck_core::decode::VideoDecoderBackend`. Bitstream parsing, DPB and
//! reference lists are portable and live in `arcen-media`; this module only
//! fills DXVA structures and drives `ID3D11VideoContext`. No `FFmpeg`, no
//! Media Foundation decoder (ADR 0024).
//!
//! The decoder runs on a D3D11 device of its own on the presenters'
//! adapter and hands pictures over through shared textures and fences
//! ([`shared`]), so no presenter can stall decode; where the driver cannot
//! share them it runs on the presenters' device instead.
//!
//! Capabilities are probed per adapter, never assumed: every advertised
//! profile needs its format, a configuration and a decoder at 3840x2160,
//! and a real keyframe of our own encoder decoded sample-exact against an
//! independent software decode. Every picture is checked against the
//! contract the wire claims for it (a contradiction is refused, never
//! presented) and keeps the timestamp and contract of the access unit that
//! decoded it, even when a later unit outputs it.

// The windows-rs D3D11 entry points take raw pointers to descriptors that
// live on the caller's stack for the duration of the call.
#![allow(clippy::borrow_as_ptr)]

pub mod backend;
pub mod device;
pub mod dxva;
pub mod probe;
pub mod readback;
pub mod session;
pub mod shared;

pub use backend::{D3d11DecoderFactory, D3d11VideoDecoder, HandOver, PRESENTER_SLOTS};
pub use device::{DecoderProfile, HardwareAdapter, VideoDevice, hardware_adapters};
pub use probe::{
    ProbeReport, probe_decode_capabilities, probe_for_adapter, probe_report_for_adapter, prove,
};
pub use session::D3d11Picture;
pub use shared::{ShareProof, SharedPicture, prove_sharing};
