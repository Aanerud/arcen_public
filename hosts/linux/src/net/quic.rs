//! QUIC endpoint management for the Linux Pier host.
//!
//! Provides utilities to build a `quinn::ServerConfig` from the existing
//! rustls TLS material and bind a
//! `quinn::Endpoint` on the configured QUIC UDP port.
//!
use std::net::SocketAddr;
use std::sync::Arc;

use quinn::{Endpoint, ServerConfig};

use crate::net::tls::TlsLifecycle;

/// Constructs a `quinn::ServerConfig` from the existing rustls material held
/// in `tls_lifecycle`.
///
/// The `rustls::ServerConfig` is built using the same cert resolver that
/// backs the shared TLS lifecycle. The ALPN is overridden to
/// Arcen's direct QUIC ALPN so plain-WS and HTTP/3 clients are rejected at
/// the QUIC crypto layer before any application data flows.
///
/// # Errors
///
/// Returns an `Err` string when rustls or Quinn config construction fails (e.g.,
/// the cert/key is not loaded yet, or the QUIC crypto provider rejects the
/// config).
pub(crate) fn build_quic_server_config(
    tls_lifecycle: &TlsLifecycle,
) -> Result<ServerConfig, String> {
    let mut rustls_config = tls_lifecycle
        .rustls_server_config_for_quic()
        .map_err(|error| format!("build QUIC rustls config: {error}"))?;
    rustls_config.alpn_protocols = vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()];

    let quic_crypto =
        quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config).map_err(|error| {
            format!("build QUIC crypto config (is rustls ring provider initialized?): {error}")
        })?;

    let transport = arcen_transport::quic::recommended_transport_config_arc(
        &arcen_transport::BoundedTransportPolicy::default(),
    );
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    config.transport_config(transport);
    arcen_transport::quic::apply_direct_server_limits(&mut config);
    Ok(config)
}

/// Resolves one host/UDP-port pair using the same DNS-capable semantics as the
/// TCP listener.
pub(crate) async fn resolve_bind_addr(host: &str, port: u16) -> Result<SocketAddr, String> {
    let mut resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("resolve QUIC bind host '{host}:{port}': {error}"))?;
    resolved
        .next()
        .ok_or_else(|| format!("resolve QUIC bind host '{host}:{port}': no addresses returned"))
}

/// Binds a `quinn::Endpoint` for QUIC accepts on `addr`.
///
/// # Errors
///
/// Returns an `Err` string on bind failure (port in use, permissions, etc.).
pub(crate) fn bind_endpoint(
    addr: SocketAddr,
    server_config: ServerConfig,
) -> Result<Endpoint, String> {
    Endpoint::server(server_config, addr).map_err(|error| format!("bind QUIC endpoint: {error}"))
}
