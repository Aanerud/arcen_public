// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The portable wgpu video paint callback (feature `wgpu-presenter`).
//!
//! Today `clients/macos/src/ui/video_render.rs` and `video_render.wgsl`:
//! the egui fallback path that draws CPU RGBA frames and the eight-bit
//! picture when no dedicated presenter is attached. It moves here so the
//! Windows Deck's egui-wgpu shell draws the same fallback the Mac does.
//! The dependency versions are pinned to the ones eframe already resolves.

pub use egui_wgpu;
pub use wgpu;
