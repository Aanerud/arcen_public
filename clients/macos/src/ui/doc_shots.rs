// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Developer-only (`doc-shots`): `arcen-deck doc-shots` renders the user
//! manual's scenes in this Deck's own egui setup and root frame
//! (`arcen_deck_core::doc_shots`, docs/testing/deck-manual-screenshots.md).

use arcen_deck_core::doc_shots::{DocPlatform, DocShotShell};

/// The macOS Deck, as the manual's screenshots draw it.
#[derive(Debug, Clone, Copy, Default)]
pub struct MacDeckShell;

impl DocShotShell for MacDeckShell {
    fn platform(&self) -> DocPlatform {
        DocPlatform::Mac
    }

    fn setup(&self, ctx: &egui::Context) {
        super::app::setup_egui(ctx);
    }

    fn root_frame(&self, ui: &egui::Ui) -> egui::Frame {
        super::app::launcher_root_frame(ui)
    }

    fn canvas_available(&self) -> bool {
        super::app::canvas_request(
            true,
            arcen_deck_core::settings::ClientSettings::default().canvas_cache_mib,
        ) != arcen_deck_core::canvas::CanvasRequest::Unavailable
    }
}

/// `arcen-deck doc-shots …`: the process exit code.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    arcen_deck_core::doc_shots::cli::run(args, &MacDeckShell)
}
