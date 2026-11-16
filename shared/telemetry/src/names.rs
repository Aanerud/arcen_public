// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Canonical schema names shared by every platform adapter.

/// Canonical tracing targets.
pub mod target {
    /// Authentication and identity binding.
    pub const AUTH: &str = "arcen::auth";
    /// Display configuration and restore.
    pub const DISPLAY: &str = "arcen::display";
    /// HID and peripheral forwarding.
    pub const HID: &str = "arcen::hid";
    /// Health and `QoS` rollups.
    pub const HEALTH: &str = "arcen::health";
    /// Media delivery and presentation.
    pub const MEDIA: &str = "arcen::media";
    /// Network path state.
    pub const NET: &str = "arcen::net";
    /// Session lifecycle.
    pub const SESSION: &str = "arcen::session";
    /// Telemetry runtime state.
    pub const TELEMETRY: &str = "arcen::telemetry";
    /// QUIC transport and lane ownership. Same value as
    /// `arcen_transport::LOG_TARGET_TRANSPORT`.
    pub const TRANSPORT: &str = "arcen::transport";
}

/// Canonical structured field keys.
pub mod field {
    /// Authentication method.
    pub const AUTH_METHOD: &str = "auth_method";
    /// Display implementation.
    pub const DISPLAY_BACKEND: &str = "display_backend";
    /// Capture-to-encoder copy path (`zero-copy-import`, `gpu-convert`,
    /// `host-copy`, `cross-gpu`).
    pub const COPY_PATH: &str = "copy_path";
    /// Linux desktop provider (`xorg`, `wayland`).
    pub const DESKTOP_PROVIDER: &str = "provider";
    /// Authenticated identity binding method.
    pub const IDENTITY_BINDING: &str = "identity_binding";
    /// QUIC lane role (`control`, `video`, `audio_output`, `input_cursor`,
    /// `microphone`, `bulk_aux`).
    pub const LANE_ROLE: &str = "role";
    /// Effective profile source.
    pub const PROFILE_SOURCE: &str = "profile_source";
    /// Bounded failure classification.
    pub const REASON_CLASS: &str = "reason_class";
    /// Telemetry sink name.
    pub const SINK: &str = "sink";
    /// Transport implementation.
    pub const TRANSPORT: &str = "transport";
}

/// Exact still image (lossless refinement, ADR 0016): one vocabulary for
/// the Pier and the Deck. Emitted on [`target::MEDIA`].
pub mod refine {
    /// The session's refinement plan was decided: `state` (`on`/`off`),
    /// `reason` when off, `domain`, `bit_depth`, `still_ms`, `max_mbps`.
    pub const PLAN: &str = "video.refine.plan";
    /// A periodic summary: records, tiles, blocks, wire bytes, coverage,
    /// verification level, digest matches and mismatches, drops.
    pub const WINDOW: &str = "video.refine.window";
    /// The Deck verified a still picture: `coverage_permille`, `level`,
    /// `since_still_ms` (time from the last change to the verdict).
    pub const VERIFIED: &str = "video.refine.verified";
    /// The Pier's refinement budget changed (ADR 0016 R6): `rate_bps`,
    /// `source` (`floor` before a path sample, `residual` after), `hold`
    /// (a suspension's reason, or none), `delivered_bps`,
    /// `interactive_bps`, `holds`, `backoffs`, `video_deferrals`.
    pub const BUDGET: &str = "video.refine.budget";
}

/// Canonical component values.
pub mod component {
    /// macOS Deck client.
    pub const DECK: &str = "deck";
    /// Pier host service.
    pub const PIER: &str = "pier";
    /// Pier per-session process.
    pub const SESSION_AGENT: &str = "session_agent";
}
