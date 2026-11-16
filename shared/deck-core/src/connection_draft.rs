// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The connection a Deck is about to make: the selected saved connection or
//! quick-connect address, and the credentials typed for it.

use crate::config::ConnectionKind;

/// A connection being prepared. The password is wiped on drop and never
/// printed by `Debug`.
#[derive(Clone)]
pub struct ConnectionDraft {
    pub saved_connection_id: Option<String>,
    pub kind: ConnectionKind,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
    /// The certificate trust a command line or an earlier session fixed for
    /// this connection (the session engine's type, so only with `runtime`).
    #[cfg(feature = "runtime")]
    pub tls_trust: Option<crate::runtime::tls::TlsTrustConfig>,
    pub auto_reconnect: bool,
}

#[allow(
    clippy::missing_fields_in_debug,
    reason = "the saved id is not printed; the password is redacted"
)]
impl std::fmt::Debug for ConnectionDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("ConnectionDraft");
        debug
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("use_tls", &self.use_tls);
        #[cfg(feature = "runtime")]
        debug.field("tls_trust", &self.tls_trust);
        debug.field("auto_reconnect", &self.auto_reconnect).finish()
    }
}

impl Drop for ConnectionDraft {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.password);
    }
}

impl ConnectionDraft {
    #[must_use]
    pub fn new(kind: ConnectionKind) -> Self {
        Self {
            saved_connection_id: None,
            kind,
            host: String::new(),
            port: kind.default_port(),
            username: String::new(),
            password: String::new(),
            use_tls: true,
            #[cfg(feature = "runtime")]
            tls_trust: None,
            auto_reconnect: true,
        }
    }

    /// `host:port`, as the trust store and the screens name the endpoint.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The name the sign-in screens show: the host, or "Pier" when there is none.
    #[must_use]
    pub fn display_name(&self) -> &str {
        if self.host.is_empty() {
            "Pier"
        } else {
            self.host.as_str()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_draft_debug_redacts_password() {
        let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
        draft.password = "dummy-password".to_string();
        let debug = format!("{draft:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("dummy-password"));
    }
}
