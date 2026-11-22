// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Developer-only (feature `doc-shots`, never in a release): the Deck
//! manual's screenshots.
//!
//! Every scene of the manual (`deck-shots/manifest.json` in the website
//! repository) is built here from the real shared types — saved
//! connections, the sign-in outcome, the certificate prompts, the
//! connection test's report, an import summary — and drawn with the real
//! shared screens inside the shell's own root frame and egui setup
//! ([`DocShotShell`]). Each image is rendered offscreen with egui-wgpu and
//! read back, so a run needs no window, no one at the desk and no screen
//! recording, and works with the screen locked. Nothing touches a network,
//! the user's settings or the keychain: addresses are placeholders that are
//! never looked up, fingerprints come from a fixed seed, and the settings
//! repository lives in memory.
//!
//! Beside the PNGs a run writes `deck-shots-geometry.json`: for every
//! scene and theme, where each of the manual's numbered hotspots sits, as a
//! percentage of the image, from the rectangles egui reports for the
//! widget the hotspot names ([`hotspots`]). A hotspot that names a group
//! rather than one widget is reported as unmapped, with its parts.
//!
//! How to run it: `docs/testing/deck-manual-screenshots.md`.

pub mod capture;
pub mod cli;
pub mod gpu;
pub mod hotspots;
pub mod scenes;
mod services;

/// The window every scene is drawn in, in points (the Decks' default
/// window size).
pub const WINDOW_POINTS: [f32; 2] = [1120.0, 760.0];

/// The scale every image is rendered at: 2240 × 1520 pixels.
pub const PIXELS_PER_POINT: f32 = 2.0;

/// Which Deck draws the scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocPlatform {
    /// The macOS Deck.
    Mac,
    /// The Windows Deck.
    Windows,
}

impl DocPlatform {
    /// The manifest's and the file names' word for it.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Mac => "mac",
            Self::Windows => "windows",
        }
    }

    /// How the shared screens name it.
    #[must_use]
    pub const fn deck_platform(self) -> crate::screens::wording::DeckPlatform {
        match self {
            Self::Mac => crate::screens::wording::DeckPlatform::Mac,
            Self::Windows => crate::screens::wording::DeckPlatform::Pc,
        }
    }
}

/// Light or dark: what the window system reports to the shell for each
/// image (the operating system's own appearance is never changed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocTheme {
    Light,
    Dark,
}

impl DocTheme {
    /// Both, in the order the images are written.
    pub const ALL: [Self; 2] = [Self::Light, Self::Dark];

    /// The manifest's and the file names' word for it.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// egui's theme.
    #[must_use]
    pub const fn egui(self) -> egui::Theme {
        match self {
            Self::Light => egui::Theme::Light,
            Self::Dark => egui::Theme::Dark,
        }
    }
}

/// What a Deck shell lends the harness, so a rendered scene is what that
/// Deck shows: the shell's own setup and root frame, never a copy.
pub trait DocShotShell {
    /// Which Deck this is.
    fn platform(&self) -> DocPlatform;

    /// The shell's egui setup, exactly as it runs when its window opens:
    /// fonts, style, both palettes and which Deck draws the screens.
    fn setup(&self, ctx: &egui::Context);

    /// The frame the shell draws Home, the certificate prompts and
    /// Settings in (the window's background).
    fn root_frame(&self, ui: &egui::Ui) -> egui::Frame;

    /// Whether this Deck's presenter keeps a canvas, which decides whether
    /// Settings › Streaming offers Canvas screen updates.
    fn canvas_available(&self) -> bool;
}

#[cfg(test)]
mod tests;
