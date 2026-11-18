// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
/// The source location and AGPL-3.0 source offer every Deck shows (moved
/// to `arcen_deck_core::about`, ADR 0024).
pub use arcen_deck_core::about::{SOURCE_OFFER, SOURCE_URL};

/// Release packaging scans the final binary for this marker and refuses to
/// ship it. It must stay compiled into every `dev-tools` binary, including
/// `--no-build` package assemblies, so stale local artefacts cannot become a
/// release.
#[cfg(feature = "dev-tools")]
#[used]
pub static ARCEN_DECK_DEV_TOOLS_FEATURE_MARKER: &[u8] =
    b"ARCEN_DECK_DEV_TOOLS_FEATURE_MARKER_DO_NOT_SHIP";

pub mod build_identity;
pub mod clipboard;
pub mod credentials;
pub mod display;
#[cfg(target_os = "macos")]
pub mod gestures;
#[cfg(feature = "experimental-raw-hid")]
pub mod hid;
pub mod installed_copies;
pub mod logging;
pub mod microphone;
pub mod netinfo;
pub mod observability;
pub mod pipeline;
pub mod platform;
pub mod platform_services;
#[cfg(feature = "dev-tools")]
pub mod probe_matrix;
pub mod protocol;
pub mod reconnect;
pub mod support;
pub mod tablet;
pub mod timezone;
pub mod transport;
pub mod ui;
#[cfg(feature = "native-tablet")]
pub mod usb_bridge;
#[cfg(feature = "usb-emulate-lab")]
pub mod usb_emulate;
#[cfg(feature = "native-tablet")]
pub mod usb_helper_client;
#[cfg(feature = "native-tablet")]
pub mod usb_helper_install;
