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
}

/// Canonical structured field keys.
pub mod field {
    /// Authentication method.
    pub const AUTH_METHOD: &str = "auth_method";
    /// Display implementation.
    pub const DISPLAY_BACKEND: &str = "display_backend";
    /// Authenticated identity binding method.
    pub const IDENTITY_BINDING: &str = "identity_binding";
    /// Effective profile source.
    pub const PROFILE_SOURCE: &str = "profile_source";
    /// Bounded failure classification.
    pub const REASON_CLASS: &str = "reason_class";
    /// Telemetry sink name.
    pub const SINK: &str = "sink";
    /// Transport implementation.
    pub const TRANSPORT: &str = "transport";
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
