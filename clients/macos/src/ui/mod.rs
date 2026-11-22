// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

pub mod app;
pub mod audio_worker;
#[cfg(target_os = "macos")]
mod canvas_metal;
pub mod colour_check;
pub mod connection_test;
pub mod display_fit;
/// Developer-only (`doc-shots`): the manual's screenshots in this Deck's
/// setup and root frame.
#[cfg(feature = "doc-shots")]
pub mod doc_shots;
pub mod home;
mod keyboard;
mod macos_menu;
pub mod media_worker;
pub mod multi_window;
pub mod multi_window_activation;
pub mod multi_window_diagnostic;
pub mod multi_window_runtime;
pub mod multi_window_session;
mod option_keys;
mod pass_pacing;
mod paste_keys;
pub(crate) mod pen_input;
#[cfg(target_os = "macos")]
mod refine_overlay;
pub mod region_runtime;
mod session_truth;
#[cfg(test)]
mod settings_page_tests;
mod sharp_cursor;
mod system_shortcut_tap;
pub mod text_input;
pub mod theme;
pub mod trusted_pins;
mod video_layer_status;
#[cfg(target_os = "macos")]
mod video_metal_layer;
/// Off macOS the dedicated Metal layers do not exist; see the module doc.
#[cfg(not(target_os = "macos"))]
#[path = "video_layer_unavailable.rs"]
mod video_metal_layer;
mod video_render;
/// Developer-only, default-off (`dev-tools`): see the module docs. Never
/// reachable from [`run_native_app`] or any production session path.
#[cfg(feature = "dev-tools")]
pub mod virtual_monitor_lab;
#[cfg(feature = "dev-tools")]
pub mod windowed_monitor_test;

pub use app::{run_native_app, AppScreen, ArcenApp};
