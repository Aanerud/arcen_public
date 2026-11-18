// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Transport-profile selection for direct sessions.
//!
//! This module introduces `DirectTransport`, a thin connector abstraction that
//! sits between `ConnectOptions` and the concrete direct-session I/O type. It
//! allows `run_session_correlated` and `connect_smoke_correlated` to select
//! a transport profile without rewriting the full session loop.
//!
//! The Deck connects over QUIC only (`AGENTS.md`); there is no other
//! transport to select or to fall back to.

#![allow(
    missing_debug_implementations,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::disallowed_methods,
    clippy::expect_used,
    clippy::large_enum_variant,
    clippy::large_futures,
    clippy::manual_let_else,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::missing_errors_doc,
    clippy::missing_fields_in_debug,
    clippy::needless_pass_by_value,
    clippy::redundant_else,
    clippy::result_large_err,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::trivially_copy_pass_by_ref,
    clippy::type_complexity,
    clippy::unnecessary_wraps,
    clippy::unwrap_used,
    clippy::assigning_clones,
    clippy::needless_continue,
    reason = "the session engine moved verbatim from the macOS Deck (ADR 0024 step 4); \
              these lints are tightened in follow-ups, never by changing behaviour in the move"
)]

use arcen_transport::TransportProfile;

use super::session::ConnectOptions;

/// Which network transport a direct session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectTransportKind {
    /// QUIC (UDP + Quinn + rustls), confirmed by both peers during the
    /// product handshake.
    Quic,
}

impl DirectTransportKind {
    /// Returns the matching `TransportProfile` for logging and telemetry.
    #[must_use]
    pub const fn transport_profile(self) -> TransportProfile {
        match self {
            Self::Quic => TransportProfile::Quic,
        }
    }

    /// Returns the string label used in structured log fields and telemetry
    /// events.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Quic => "quic",
        }
    }
}

/// Resolves the transport kind for an outbound direct-session attempt:
/// always QUIC.
pub fn resolve_transport(options: &ConnectOptions) -> Result<DirectTransportKind, &'static str> {
    let _ = options;
    Ok(DirectTransportKind::Quic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_telemetry::ClientTelemetry;
    use crate::runtime::session::StreamProfile;
    use crate::runtime::tls::TlsTrustConfig;
    use arcen_protocol::messages::{CursorMode, TabletModeMsg};
    use std::sync::Arc;
    use std::time::Duration;

    fn options() -> ConnectOptions {
        ConnectOptions {
            display_refresh: None,
            services: crate::runtime::test_services(),
            host: "host.example".to_string(),
            port: 18_444,
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(1),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: true,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            exact_still_image: false,
        }
    }

    #[test]
    fn quic_profile_label_and_transport_agree() {
        assert_eq!(DirectTransportKind::Quic.label(), "quic");
        assert_eq!(
            DirectTransportKind::Quic.transport_profile(),
            TransportProfile::Quic
        );
    }

    #[test]
    fn resolving_always_selects_quic() {
        assert_eq!(
            resolve_transport(&options()).unwrap(),
            DirectTransportKind::Quic
        );
    }
}
