// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Moved to [`arcen_deck_core::runtime::monitor_router`] (ADR 0024
//! extraction step 5); the macOS Deck's routers default to its
//! VideoToolbox decoder.

pub use arcen_deck_core::runtime::monitor_router::*;

/// Per-monitor decoders, VideoToolbox by default.
pub type MonitorFrameRouter<D = crate::pipeline::video_decoder::NativeVideoDecoder> =
    arcen_deck_core::runtime::monitor_router::MonitorFrameRouter<D>;
