// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The QUIC endpoint every Pier listens with.

use std::net::SocketAddr;
use std::sync::Arc;

use super::DIRECT_QUIC_ALPN_PROTOCOL;
use crate::BoundedTransportPolicy;

/// A Pier's QUIC server over `tls` (usually
/// [`crate::host_material::HostTlsMaterial::server_config`]): Arcen's ALPN
/// only, so HTTP/3 and WebSocket clients are refused in the handshake; the
/// lanes a Deck opens plus one replacement of each shape (ADR 0014); and the
/// pre-authentication limits.
///
/// # Errors
///
/// Returns a message when Quinn refuses the TLS configuration.
pub fn pier_server_config(mut tls: rustls::ServerConfig) -> Result<quinn::ServerConfig, String> {
    tls.alpn_protocols = vec![DIRECT_QUIC_ALPN_PROTOCOL.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|error| format!("build QUIC crypto config: {error}"))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(super::lane_transport_config_arc(
        &BoundedTransportPolicy::default(),
        arcen_protocol::lanes::LaneEndpoint::Pier,
    ));
    super::apply_direct_server_limits(&mut config);
    Ok(config)
}

/// Resolves a configured listen host (a name or an address) and port to the
/// first address the resolver returns.
///
/// # Errors
///
/// Returns a message when the name does not resolve.
pub async fn resolve_pier_bind_address(host: &str, port: u16) -> Result<SocketAddr, String> {
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("resolve QUIC bind host '{host}:{port}': {error}"))?
        .next()
        .ok_or_else(|| format!("resolve QUIC bind host '{host}:{port}': no addresses returned"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pier_server_speaks_only_arcen_alpn() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = rcgen::KeyPair::generate().expect("key");
        let certificate = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .expect("params")
            .self_signed(&key)
            .expect("certificate");
        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
            .expect("tls");
        tls.alpn_protocols = vec![b"h3".to_vec()];
        assert!(pier_server_config(tls).is_ok());
    }

    #[tokio::test]
    async fn listen_hosts_resolve_like_the_listener() {
        assert_eq!(
            resolve_pier_bind_address("127.0.0.1", 18444).await,
            Ok("127.0.0.1:18444".parse().expect("address"))
        );
        assert!(
            resolve_pier_bind_address("not a host name", 18444)
                .await
                .is_err()
        );
    }
}
