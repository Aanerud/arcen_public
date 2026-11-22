// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Developer-only (`doc-shots`): `arcen-deck.exe doc-shots` renders the user
//! manual's scenes in this Deck's own egui setup and root frame
//! (`arcen_deck_core::doc_shots`, docs/testing/deck-manual-screenshots.md).

use arcen_deck_core::doc_shots::{DocPlatform, DocShotShell};

/// The Windows Deck, as the manual's screenshots draw it.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsDeckShell;

impl DocShotShell for WindowsDeckShell {
    fn platform(&self) -> DocPlatform {
        DocPlatform::Windows
    }

    fn setup(&self, ctx: &egui::Context) {
        super::setup_egui(ctx);
    }

    fn root_frame(&self, ui: &egui::Ui) -> egui::Frame {
        super::launcher::launcher_root_frame(ui)
    }

    fn canvas_available(&self) -> bool {
        arcen_deck_core::canvas::CanvasRequest::for_sink::<crate::media::WindowSink>(
            true,
            arcen_deck_core::settings::ClientSettings::default().canvas_cache_mib,
        ) != arcen_deck_core::canvas::CanvasRequest::Unavailable
    }
}

/// `arcen-deck.exe doc-shots …`: the process exit code.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    arcen_deck_core::doc_shots::cli::run(args, &WindowsDeckShell)
}
