// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

pub mod app;
pub mod audio_worker;
pub mod colour_check;
pub mod connection_test;
pub mod display_fit;
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
mod paste_keys;
pub(crate) mod pen_input;
pub mod region_runtime;
mod session_truth;
mod sharp_cursor;
pub mod text_input;
pub mod theme;
pub mod trusted_pins;
mod video_metal_layer;
mod video_render;
/// Developer-only, default-off (`dev-tools`): see the module docs. Never
/// reachable from [`run_native_app`] or any production session path.
#[cfg(feature = "dev-tools")]
pub mod virtual_monitor_lab;
#[cfg(feature = "dev-tools")]
pub mod windowed_monitor_test;

pub use app::{run_native_app, AppScreen, ArcenApp};
