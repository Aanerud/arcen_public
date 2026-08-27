//! TLS peer identity and session/role authorization hook.
//!
//! This module intentionally ships no permissive default implementation
//! (for example, an "allow all" authorizer). Callers must supply a
//! [`PeerIdentityAuthorizer`] that inspects the peer's TLS certificate chain
//! together with the role, session, and grant-bound identity it claims during the binding
//! handshake, and explicitly decides whether the connection may proceed.
//! Certificate issuance, rotation, and revocation remain the caller's
//! concern; this hook only authorizes an already-validated rustls chain
//! against Arcen-level role/session/identity expectations.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;

use rustls::pki_types::CertificateDer;

use crate::PeerRole;

/// Product-neutral peer role, usable by Host, Client, and Gateway adapters
/// without this crate depending on any of those product crates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuicRole {
    /// Streaming host workstation agent.
    Host,
    /// Desktop client application.
    Client,
    /// Stateless connection/policy gateway.
    Gateway,
}

impl Display for QuicRole {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Host => "host",
            Self::Client => "client",
            Self::Gateway => "gateway",
        };
        formatter.write_str(label)
    }
}

impl From<QuicRole> for PeerRole {
    fn from(role: QuicRole) -> Self {
        match role {
            QuicRole::Host => Self::Host,
            QuicRole::Client => Self::Client,
            QuicRole::Gateway => Self::Gateway,
        }
    }
}

impl From<PeerRole> for QuicRole {
    fn from(role: PeerRole) -> Self {
        match role {
            PeerRole::Host => Self::Host,
            PeerRole::Client => Self::Client,
            PeerRole::Gateway => Self::Gateway,
        }
    }
}

/// Everything available to authorize a peer during the binding handshake.
#[derive(Debug, Clone, Copy)]
pub struct PeerIdentityClaim<'a> {
    /// Peer TLS certificate chain, end-entity certificate first, as returned
    /// by `quinn::Connection::peer_identity()` downcast to
    /// `Vec<rustls::pki_types::CertificateDer<'static>>`.
    pub certificate_chain: &'a [CertificateDer<'static>],
    /// Role the peer claims in its handshake request.
    pub claimed_role: QuicRole,
    /// Session identifier the peer claims to bind to.
    pub claimed_session_id: &'a str,
    /// Grant-bound peer identity the TLS chain must authenticate.
    pub expected_peer_identity: &'a str,
    /// Observed remote socket address of the connection.
    pub remote_address: SocketAddr,
}

/// Required, explicit authorization hook for TLS peer identity plus claimed
/// role/session/identity. There is no permissive default: every QUIC peer must be
/// constructed with a concrete implementation.
pub trait PeerIdentityAuthorizer: Send + Sync {
    /// Authorizes (or rejects) a peer's claimed identity/role/session.
    ///
    /// # Errors
    ///
    /// Returns an explicit rejection reason. Implementations must not panic
    /// on malformed certificate data; return
    /// [`IdentityAuthorizationError::Rejected`] instead.
    fn authorize(&self, claim: &PeerIdentityClaim<'_>) -> Result<(), IdentityAuthorizationError>;
}

/// Explicit identity authorization failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityAuthorizationError {
    /// The authorizer rejected the peer for an implementation-defined reason.
    Rejected(String),
    /// No certificate chain was presented to authorize.
    NoCertificateChain,
}

impl Display for IdentityAuthorizationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(reason) => write!(formatter, "peer identity rejected: {reason}"),
            Self::NoCertificateChain => {
                formatter.write_str("peer presented no certificate chain to authorize")
            }
        }
    }
}

impl Error for IdentityAuthorizationError {}

#[cfg(test)]
mod tests {
    use super::*;

    struct RejectAll;

    impl PeerIdentityAuthorizer for RejectAll {
        fn authorize(
            &self,
            _claim: &PeerIdentityClaim<'_>,
        ) -> Result<(), IdentityAuthorizationError> {
            Err(IdentityAuthorizationError::Rejected("test".to_owned()))
        }
    }

    #[test]
    fn authorizer_is_required_to_make_an_explicit_decision() {
        let authorizer = RejectAll;
        let claim = PeerIdentityClaim {
            certificate_chain: &[],
            claimed_role: QuicRole::Host,
            claimed_session_id: "session-1",
            expected_peer_identity: "host-1",
            remote_address: "127.0.0.1:0".parse().expect("addr"),
        };
        assert_eq!(
            authorizer.authorize(&claim),
            Err(IdentityAuthorizationError::Rejected("test".to_owned()))
        );
    }
}
