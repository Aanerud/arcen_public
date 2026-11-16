// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Production live-session wiring for Carrier A (`MuxedReliableStream`)
//! multi-monitor-v1 presentation: moved to
//! [`arcen_deck_core::multi_window::activation`] so both Decks share it, and
//! keyed here by `egui::ViewportId`.

pub use arcen_deck_core::multi_window::activation::{
    teardown_required, MultiWindowEntryError, MULTI_WINDOW_ENTER_TIMEOUT,
};

use crate::protocol::messages::{MultiMonitorCarrierMsg, ServerHelloMsg};
use crate::ui::multi_window_runtime::{viewport_id_for, MultiWindowPlan};
use crate::ui::multi_window_session::ValidatedAppliedTopology;

/// The live per-session multi-window state, keyed by egui viewport.
pub type MultiWindowSessionState =
    arcen_deck_core::multi_window::activation::MultiWindowSessionState<egui::ViewportId>;

/// Validates an applied `ServerHello` topology and builds its plan, with the
/// root viewport presenting the primary.
///
/// # Errors
///
/// See [`MultiWindowEntryError`].
pub fn begin_multi_window_entry(
    hello: &ServerHelloMsg,
    supported_carriers: &[MultiMonitorCarrierMsg],
) -> Result<Option<(ValidatedAppliedTopology, MultiWindowPlan)>, MultiWindowEntryError> {
    arcen_deck_core::multi_window::activation::begin_multi_window_entry(
        hello,
        supported_carriers,
        egui::ViewportId::ROOT,
        viewport_id_for,
    )
}
