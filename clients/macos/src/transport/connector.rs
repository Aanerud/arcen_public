//! Transport-profile selection for direct sessions.
//!
//! This module introduces `DirectTransport`, a thin connector abstraction that
//! sits between `ConnectOptions` and the concrete direct-session I/O type. It
//! allows `run_session_correlated` and `connect_smoke_correlated` to select
//! a transport profile without rewriting the full session loop.
//!
//! Product builds contain QUIC only. Dormant WSS compatibility builds require
//! the explicit `wss-compat` Cargo feature, and QUIC never silently downgrades.

use arcen_transport::TransportProfile;

use super::websocket::ConnectOptions;

/// Which network transport to use for a direct session connection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectTransportKind {
    /// Dormant secure-WebSocket compatibility path.
    #[cfg(feature = "wss-compat")]
    WebSocket,
    /// QUIC (UDP + Quinn + rustls), selected explicitly before dialing and
    /// confirmed by both peers during the product handshake.
    Quic,
}

impl DirectTransportKind {
    /// Selects a transport kind for `options`.
    ///
    /// Product builds always return `Quic`. Compatibility builds honor the
    /// legacy `quic_enabled` selector.
    #[must_use]
    pub fn select_for(options: &ConnectOptions) -> Self {
        #[cfg(feature = "wss-compat")]
        if !options.quic_enabled {
            return Self::WebSocket;
        }
        #[cfg(not(feature = "wss-compat"))]
        let _ = options;
        Self::Quic
    }

    /// Returns the matching `TransportProfile` for logging and telemetry.
    #[must_use]
    pub const fn transport_profile(self) -> TransportProfile {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::WebSocket => TransportProfile::WebSocketSecure,
            Self::Quic => TransportProfile::Quic,
        }
    }

    /// Returns the string label used in structured log fields and telemetry
    /// events.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::WebSocket => "wss",
            Self::Quic => "quic",
        }
    }
}

/// Resolves the transport kind for an outbound direct-session attempt.
///
pub fn resolve_transport(options: &ConnectOptions) -> Result<DirectTransportKind, &'static str> {
    Ok(DirectTransportKind::select_for(options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::ClientTelemetry;
    use crate::transport::tls::TlsTrustConfig;
    use crate::transport::websocket::StreamProfile;
    use arcen_protocol::messages::{CursorMode, TabletModeMsg};
    use std::sync::Arc;
    use std::time::Duration;

    fn options(quic_enabled: bool) -> ConnectOptions {
        ConnectOptions {
            host: "host.example".to_string(),
            port: 18_443,
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
            quic_enabled,
        }
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn websocket_profile_label_and_transport_agree() {
        assert_eq!(DirectTransportKind::WebSocket.label(), "wss");
        assert_eq!(
            DirectTransportKind::WebSocket.transport_profile(),
            TransportProfile::WebSocketSecure
        );
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
    fn resolving_quic_succeeds() {
        assert_eq!(
            resolve_transport(&options(true)).unwrap(),
            DirectTransportKind::Quic
        );
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn resolving_wss_succeeds() {
        assert_eq!(
            resolve_transport(&options(false)).unwrap(),
            DirectTransportKind::WebSocket
        );
    }
}
