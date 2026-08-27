//! QUIC multi-monitor carrier benchmark CLI: a diagnostic-only comparison
//! of Carrier A (all monitor frames multiplexed over one reliable stream)
//! and Carrier B (one reliable stream per monitor) over real, local Quinn
//! connections.
//!
//! This binary is a measurement tool, not a product carrier selector. See
//! `shared/transport/src/quic/carrier_bench.rs`'s module documentation and
//! `docs/architecture/transport.md` for the full methodology and non-claims
//! (not glass-to-glass, localhost/single-process only, no unverified
//! numeric thresholds). Real hardware (pier-linux.example.internal / an actual Deck client)
//! end-to-end validation is required before any production carrier
//! selection.
//!
//! ```text
//! cargo run --release --features quic --example quic_multi_monitor_carrier_bench -- \
//!     --monitors 4 --frames 2000 --payload-bytes 65536 --pattern all-active --carrier both
//! ```
//!
//! Recognized flags: `--monitors <2|4>`, exactly one of `--frames <N>` (an
//! exact, deterministic, unpaced tick count) or `--duration <e.g.
//! 10s|250ms>` (wall-clock paced — the run's production phase takes
//! approximately this long, spanning the full requested interval
//! end-to-end, not however long blasting its resolved tick count would
//! otherwise take), `--payload-bytes <N>`, `--pattern
//! <all-active|one-active-rest-idle>`, `--receiver-delay-ms <N>` (optional,
//! default `0`; a carrier-neutral, per-monitor, post-demux
//! consumer/validation delay applied identically by both carriers, so the
//! same value produces a comparable predicted completion floor for either
//! one), `--carrier <a|b|both>` (optional, default `both`). Every run is
//! additionally bounded by its own predicted completion deadline
//! (production/transfer time, plus any `receiver_delay` floor sized to the
//! single busiest monitor's own frame count, plus a small fixed drain
//! allowance); a pathological `--duration`/`--receiver-delay-ms`
//! combination is rejected up front by argument validation rather than
//! left to actually run for an unreasonable amount of time. This binary
//! always writes stable JSON to stdout — there is no `--json` flag, JSON
//! emission is not optional or gated — and a concise human-readable
//! summary to stderr, so `--carrier both > result.json` captures only the
//! JSON.
//!
//! This is a standalone diagnostic binary (not a library `#[cfg(test)]`
//! module), so it is not covered by `arcen_transport`'s crate-level
//! `#[cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]`.
//! Its setup path panics loudly on failure by design: a benchmark harness
//! that silently limped on with degraded certs/config would produce
//! numbers nobody should trust, so `expect` with a descriptive message is
//! the correct failure mode here, matching `tests/quic_monitor_stream.rs`'s
//! precedent for connection-setup helpers.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::Arc;

use arcen_transport::BoundedTransportPolicy;
use arcen_transport::quic::{
    CarrierKind, CarrierRunResult, CarrierSelection, ComparisonResult,
    monitor_carrier_transport_config_arc, recommended_transport_config_arc, run_carrier_a,
    run_carrier_b,
};
use quinn::Connection;
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

/// ALPN used only by this diagnostic tool's ephemeral loopback connections.
const ALPN: &[u8] = b"arcen-quic-carrier-bench/1";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match arcen_transport::quic::parse_cli_args(&args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            eprintln!(
                "usage: quic_multi_monitor_carrier_bench --monitors <2|4> \
                 (--frames <N> | --duration <e.g. 10s>) --payload-bytes <N> \
                 --pattern <all-active|one-active-rest-idle> \
                 [--receiver-delay-ms <N>] [--carrier <a|b|both>]"
            );
            std::process::exit(2);
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let comparison = runtime.block_on(async {
        let carrier_a = if matches!(config.carriers, CarrierSelection::Both)
            || config.carriers == CarrierSelection::Only(CarrierKind::A)
        {
            Some(run_one_carrier_a(&config).await)
        } else {
            None
        };
        let carrier_b = if matches!(config.carriers, CarrierSelection::Both)
            || config.carriers == CarrierSelection::Only(CarrierKind::B)
        {
            Some(run_one_carrier_b(&config).await)
        } else {
            None
        };
        ComparisonResult {
            config,
            carrier_a,
            carrier_b,
        }
    });

    println!("{}", comparison.to_json());
    eprintln!("{}", comparison.human_summary());
}

async fn run_one_carrier_a(config: &arcen_transport::quic::BenchConfig) -> CarrierRunResult {
    // Carrier A's single multiplexed stream only ever needs the live
    // product `recommended_transport_config` (uni-stream limit of 1).
    let pair = connected_pair(recommended_transport_config_arc).await;
    run_carrier_a(&pair.client_connection, &pair.server_connection, config)
        .await
        .expect("carrier A run")
}

async fn run_one_carrier_b(config: &arcen_transport::quic::BenchConfig) -> CarrierRunResult {
    // Carrier B needs the test-only `monitor_carrier_transport_config`
    // (concurrent uni-stream limit raised to 4) to open one stream per
    // monitor on a single connection.
    let pair = connected_pair(monitor_carrier_transport_config_arc).await;
    run_carrier_b(&pair.client_connection, &pair.server_connection, config)
        .await
        .expect("carrier B run")
}

/// A connected client/server QUIC connection pair with no admission or
/// binding-handshake layer above it — this diagnostic exercises the raw
/// carrier foundations directly, the same way the crate's own loopback
/// tests do.
struct ConnectedPair {
    #[allow(dead_code)]
    client_endpoint: quinn::Endpoint,
    #[allow(dead_code)]
    server_endpoint: quinn::Endpoint,
    client_connection: Connection,
    server_connection: Connection,
}

async fn connected_pair(
    transport_config: fn(&BoundedTransportPolicy) -> Arc<quinn::TransportConfig>,
) -> ConnectedPair {
    let policy = BoundedTransportPolicy::default();
    let server_identity = generate_ephemeral_identity();
    let client_identity = generate_ephemeral_identity();

    let server_config = build_server_config(
        &server_identity,
        &client_identity.cert_der,
        &policy,
        transport_config,
    );
    let client_config = build_client_config(
        &client_identity,
        &server_identity.cert_der,
        &policy,
        transport_config,
    );

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("bind addr");
    let server_endpoint =
        quinn::Endpoint::server(server_config, bind_addr).expect("server endpoint");
    let server_addr = server_endpoint.local_addr().expect("server local addr");
    let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().expect("client bind addr"))
        .expect("client endpoint");

    let server_endpoint_for_task = server_endpoint.clone();
    let accept_task = tokio::spawn(async move {
        let incoming = server_endpoint_for_task
            .accept()
            .await
            .expect("incoming connection");
        incoming.await.expect("accepted connection")
    });

    let client_connection = client_endpoint
        .connect_with(client_config, server_addr, "localhost")
        .expect("start client connection")
        .await
        .expect("client connection");
    let server_connection = accept_task.await.expect("accept task join");

    ConnectedPair {
        client_endpoint,
        server_endpoint,
        client_connection,
        server_connection,
    }
}

/// One ephemeral, process-local self-signed identity generated fresh for
/// this run via `rcgen`; never persisted, never a product certificate.
struct EphemeralIdentity {
    cert_der: CertificateDer<'static>,
    key_der: PrivatePkcs8KeyDer<'static>,
}

fn generate_ephemeral_identity() -> EphemeralIdentity {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("generate ephemeral self-signed certificate");
    EphemeralIdentity {
        cert_der: cert.der().clone(),
        key_der: PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
    }
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn build_server_config(
    server: &EphemeralIdentity,
    trusted_client_cert: &CertificateDer<'static>,
    policy: &BoundedTransportPolicy,
    transport_config: fn(&BoundedTransportPolicy) -> Arc<quinn::TransportConfig>,
) -> quinn::ServerConfig {
    let mut client_roots = RootCertStore::empty();
    client_roots
        .add(trusted_client_cert.clone())
        .expect("add trusted client root");
    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(client_roots))
        .build()
        .expect("build client cert verifier");

    let mut rustls_config = rustls::ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(
            vec![server.cert_der.clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(server.key_der.clone_key()),
        )
        .expect("server rustls config");
    rustls_config.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config)
        .expect("quic server crypto config");
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    config.transport_config(transport_config(policy));
    config
}

fn build_client_config(
    client: &EphemeralIdentity,
    trusted_server_cert: &CertificateDer<'static>,
    policy: &BoundedTransportPolicy,
    transport_config: fn(&BoundedTransportPolicy) -> Arc<quinn::TransportConfig>,
) -> quinn::ClientConfig {
    let mut server_roots = RootCertStore::empty();
    server_roots
        .add(trusted_server_cert.clone())
        .expect("add trusted server root");

    let mut rustls_config = rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(server_roots)
        .with_client_auth_cert(
            vec![client.cert_der.clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(client.key_der.clone_key()),
        )
        .expect("client rustls config");
    rustls_config.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config)
        .expect("quic client crypto config");
    let mut config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    config.transport_config(transport_config(policy));
    config
}
