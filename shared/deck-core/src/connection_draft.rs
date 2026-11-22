// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The connection a Deck is about to make: the selected saved connection or
//! quick-connect address, and the credentials typed for it.

use crate::config::{ConnectionKind, SavedConnectionSummary, normalized_connection_host};

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

    /// Whether this draft is `saved`'s, at the address `saved` has now
    /// (its host compared as the trust store and the matcher compare it).
    #[must_use]
    pub fn is_for(&self, saved: &SavedConnectionSummary) -> bool {
        self.saved_connection_id.as_deref() == Some(saved.id.as_str())
            && normalized_connection_host(&self.host) == normalized_connection_host(&saved.host)
            && self.port == saved_port(saved)
            && self.use_tls == saved.use_tls
    }

    /// Points the draft at `saved`, as selecting it on Home does: its
    /// address and remembered username, and no password. Anything fixed
    /// for another endpoint (a command line's or an earlier session's
    /// certificate trust) stays with that endpoint: unless the draft is
    /// already `saved`'s at the same address (a retry), selecting starts
    /// a fresh draft. Only the Auto-reconnect choice carries over.
    pub fn select_saved(&mut self, saved: &SavedConnectionSummary) {
        if !self.is_for(saved) {
            let auto_reconnect = self.auto_reconnect;
            *self = Self::new(saved.kind);
            self.auto_reconnect = auto_reconnect;
        }
        self.saved_connection_id = Some(saved.id.clone());
        self.kind = saved.kind;
        self.host.clone_from(&saved.host);
        self.port = saved_port(saved);
        self.use_tls = saved.use_tls;
        self.username = saved
            .settings
            .remembered_username
            .clone()
            .unwrap_or_default();
        zeroize::Zeroize::zeroize(&mut self.password);
    }

    /// What Connect on `saved`'s card dials: the username and password
    /// typed on it, to the address the saved record has now (never an
    /// address the draft kept from before an edit). A certificate trust
    /// the draft carries goes with it only when the draft is already
    /// `saved`'s at that address.
    #[must_use]
    pub fn for_saved(&self, saved: &SavedConnectionSummary) -> Self {
        let mut draft = self.clone();
        if !self.is_for(saved) {
            draft.forget_endpoint_trust();
        }
        draft.saved_connection_id = Some(saved.id.clone());
        draft.kind = saved.kind;
        draft.host.clone_from(&saved.host);
        draft.port = saved_port(saved);
        draft.use_tls = saved.use_tls;
        draft
    }

    /// A command line's connection (`--connect`) as the saved connection
    /// it names, when exactly one does ([`crate::config::find_saved_connection_index`],
    /// as on every Deck): the draft takes that connection's id, so its own
    /// values (a withheld clipboard, Displays, the keyboard) apply, and its
    /// saved port and encryption. The host as typed, the username and a pin
    /// the command line fixed are kept. Returns the saved connection's index.
    pub fn match_saved(
        &mut self,
        saved: &[SavedConnectionSummary],
        security_mode: crate::settings::SecurityMode,
    ) -> Option<usize> {
        let index = crate::config::find_saved_connection_index(
            saved,
            self.kind,
            &self.host,
            self.port,
            self.use_tls,
            crate::config::SavedTransport::Quic,
            security_mode,
        )?;
        let connection = &saved[index];
        self.saved_connection_id = Some(connection.id.clone());
        self.port = saved_port(connection);
        self.use_tls = connection.use_tls;
        Some(index)
    }

    /// Drops what was fixed for the endpoint this draft named before: the
    /// certificate trust (only with `runtime`).
    pub fn forget_endpoint_trust(&mut self) {
        #[cfg(feature = "runtime")]
        {
            self.tls_trust = None;
        }
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

/// `saved`'s port, or its kind's when the record has none.
fn saved_port(saved: &SavedConnectionSummary) -> u16 {
    if saved.port != 0 {
        saved.port
    } else {
        saved.kind.default_port()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(id: &str, host: &str) -> SavedConnectionSummary {
        SavedConnectionSummary {
            id: id.to_owned(),
            kind: ConnectionKind::DirectMachine,
            name: id.to_owned(),
            host: host.to_owned(),
            port: 18_444,
            use_tls: true,
            transport: crate::config::SavedTransport::Quic,
            identity_security_mode: None,
            settings: crate::config::SavedConnectionSettings::default(),
            unknown: serde_json::Map::new(),
        }
    }

    #[test]
    fn a_command_line_connection_takes_the_saved_one_it_names_and_keeps_its_pin() {
        use crate::settings::SecurityMode;
        let list = [
            saved("studio", "studio.example.internal"),
            saved("render", "203.0.113.11"),
        ];
        let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
        draft.host = "Studio.example.internal.".to_owned();
        #[cfg(feature = "runtime")]
        {
            draft.tls_trust = Some(crate::runtime::tls::TlsTrustConfig::pinned_spki([5; 32]));
        }
        assert_eq!(draft.match_saved(&list, SecurityMode::Medium), Some(0));
        assert_eq!(draft.saved_connection_id.as_deref(), Some("studio"));
        assert_eq!(draft.host, "Studio.example.internal.");
        #[cfg(feature = "runtime")]
        assert!(draft.tls_trust.is_some(), "the command line's pin stays");
        // Two saved connections to one address: none is guessed.
        let twice = [
            list[0].clone(),
            saved("studio-2", "studio.example.internal"),
        ];
        let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
        draft.host = "studio.example.internal".to_owned();
        assert_eq!(draft.match_saved(&twice, SecurityMode::Medium), None);
        assert_eq!(draft.saved_connection_id, None);
    }

    #[test]
    fn connection_draft_debug_redacts_password() {
        let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
        draft.password = "dummy-password".to_string();
        let debug = format!("{draft:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("dummy-password"));
    }
}
