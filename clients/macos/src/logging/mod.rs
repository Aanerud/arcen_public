// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's log: the runtime moved to [`arcen_deck_core::logging`] (ADR
//! 0024 extraction step 4); the macOS Deck keeps only where logs live and
//! which build facts the start record names.

pub mod diagnostics;

use std::path::PathBuf;

pub use arcen_deck_core::logging::*;

/// Installs the process-wide log in [`log_dir`] and returns the directory.
pub fn init(profile: arcen_telemetry::OperationalProfile) -> Option<PathBuf> {
    let build = crate::build_identity::DeckBuildFacts::running();
    arcen_deck_core::logging::init(LogInit {
        profile,
        dir: log_dir(),
        platform: arcen_telemetry::TelemetryPlatform::Macos,
        start_facts: vec![
            ("bundle_short_version", build.bundle_short_version),
            ("bundle_version", build.bundle_version),
            ("build_id", build.build_id),
            ("source_revision", build.source_revision),
            ("bundle_path", build.bundle_path),
        ],
    })
}

/// The directory logs are written to, or would be.
#[must_use]
pub fn current_log_dir() -> Option<PathBuf> {
    active_log_dir().or_else(log_dir)
}

/// `ARCEN_LOG_DIR`, else the platform's log folder.
#[must_use]
pub fn log_dir() -> Option<PathBuf> {
    arcen_deck_core::directories::DeckDirectories::from_process_env(None, native_log_dir()).logs
}

fn native_log_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library").join("Logs").join("Arcen"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(home.join(".local").join("state").join("arcen").join("logs"))
    }
}
