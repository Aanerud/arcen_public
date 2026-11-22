// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The shared Deck screens (feature `egui`).
//!
//! Extraction step 6 moves the macOS Deck's home, settings,
//! certificate, reconnect, signing-in and statistics screens here as
//! view-state plus action enums, so both Decks render the same UI with the
//! same wording (1:1 look and feel). This feature depends on
//! egui only: no eframe, no wgpu, no window system.

#![allow(
    clippy::too_many_lines,
    reason = "a screen is drawn top to bottom in one function; splitting it hides the layout"
)]

pub mod about;
pub mod certificate;
pub mod colour_check;
pub mod connecting;
pub mod connection_test;
pub mod fidelity;
pub mod help_windows;
pub mod home;
pub mod host_pointer;
pub mod icons;
pub mod input_choices;
pub mod keyboard_help;
pub mod launcher;
pub mod list_picker;
pub mod menu;
pub mod profile;
pub mod reconnect;
pub mod settings;
pub mod sign_in;
pub mod statistics;
pub mod support_bundle;
pub mod text_input;
pub mod theme;
pub mod widgets;
pub mod wording;

use crate::present::RepaintRequester;

/// The UI thread's egui context wakes the UI for the media worker: the root
/// viewport's pass, which takes the media worker's output, even when the
/// wake lands while another viewport is being drawn.
impl RepaintRequester for egui::Context {
    fn request_repaint(&self) {
        self.request_repaint_of(egui::ViewportId::ROOT);
    }
}
