// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Log targets of the portable Deck: the same strings the macOS Deck has
//! always used, so moved code logs exactly where it did before.

/// Connection and transport.
pub const TRANSPORT: &str = arcen_telemetry::names::target::NET;
/// Certificates and pins.
pub const TLS: &str = arcen_telemetry::names::target::AUTH;
/// Session lifecycle.
pub const SESSION: &str = arcen_telemetry::names::target::SESSION;
/// Video.
pub const VIDEO: &str = arcen_telemetry::names::target::MEDIA;
/// Audio.
pub const AUDIO: &str = arcen_telemetry::names::target::MEDIA;
/// USB and HID.
pub const USB: &str = arcen_telemetry::names::target::HID;
/// The Deck's screens and settings.
pub const UI: &str = "arcen::ui";
/// Keyboard, pointer and pen.
pub const INPUT: &str = "arcen::input";
/// Health.
pub const HEALTH: &str = arcen_telemetry::names::target::HEALTH;
/// Authentication.
pub const AUTH: &str = arcen_telemetry::names::target::AUTH;
