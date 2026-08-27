//! Shared real-TLS/Quinn test fixtures for `arcen-transport`'s `--features
//! quic` integration tests.
//!
//! Uses real mutual TLS with static test-only certificates and a private
//! `rustls::RootCertStore` on both sides (never a "skip verification"
//! helper). Each integration test binary that includes this module via
//! `mod support;` gets its own copy, so not every function is necessarily
//! used by every binary.
#![allow(dead_code)]

use std::sync::Arc;

use arcen_transport::BoundedTransportPolicy;
use arcen_transport::quic::{
    monitor_carrier_transport_config_arc, recommended_transport_config_arc,
};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// ALPN shared by every real-TLS QUIC test fixture in this crate.
pub(crate) const ALPN: &[u8] = b"arcen-quic-test/1";

// Static test-only identities avoid runtime certificate generation and keep
// the security tests independent of certificate-generator dependencies.
const SERVER_CERT_HEX: &str = concat!(
    "3082018330820129a003020102020101300a06082a8648ce3d04030230143112301006035504030c",
    "096c6f63616c686f73743020170d3236303731333130313634315a180f3231323630363139313031",
    "3634315a30143112301006035504030c096c6f63616c686f73743059301306072a8648ce3d020106",
    "082a8648ce3d0301070342000415b42205ac550cc425126f143213a7e46fca470107e6586b15c849",
    "9d76cd0ff4e8f58b2081e11c2dd549532da3eb1ff1a255cd1b4eb601ca95c60545fc84dceca36a",
    "3068300c0603551d130101ff0402300030140603551d11040d300b82096c6f63616c686f73743013",
    "0603551d25040c300a06082b06010505070301300e0603551d0f0101ff040403020780301d060355",
    "1d0e041604147c8629e25e0e2cae4ffc52ebe7a0d765902366e4300a06082a8648ce3d04030203",
    "4800304502206f8ebdacb2df78719a9e6fce79884f28002dd97e08527168af364e80c21601730221",
    "0081f225dc238122c632fe433ff564535c3c5699387aa024ff91ed01293f2cc708",
);
const SERVER_KEY_HEX: &str = concat!(
    "308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b0201010420f4fd5a82",
    "95ffc85b03f74b06d58fce5dfb04c05c27a57ed5dfb6545912d89ec6a1440342000415b42205ac55",
    "0cc425126f143213a7e46fca470107e6586b15c8499d76cd0ff4e8f58b2081e11c2dd549532da3eb",
    "1ff1a255cd1b4eb601ca95c60545fc84dcec",
);
const CLIENT_CERT_HEX: &str = concat!(
    "3082017e30820123a003020102020102300a06082a8648ce3d040302301c311a301806035504030c",
    "11617263656e2d746573742d636c69656e743020170d3236303731333130313634315a180f323132",
    "36303631393130313634315a301c311a301806035504030c11617263656e2d746573742d636c6965",
    "6e743059301306072a8648ce3d020106082a8648ce3d030107034200048e63289666cf24684c9a23",
    "004d84101d1a730f55636b24354f35ded6da33377a5e668cd8a8c454f29bafc6a63e64073af7119",
    "f009d11dc334d28aa326546cea3a3543052300c0603551d130101ff0402300030130603551d25040c",
    "300a06082b06010505070302300e0603551d0f0101ff040403020780301d0603551d0e041604146a",
    "c31fb401f1f96b058c1a0dc56afc8b38aafd2d300a06082a8648ce3d0403020349003046022100",
    "fe0c50c774c80ef6e5f156059f86c30078da540938bac026176459107f2b43ba022100ae80415667",
    "f4c6a568b69291df67a00861024fca9096114584609f0461e4e1b9",
);
const CLIENT_KEY_HEX: &str = concat!(
    "308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b0201010420f97fbd95",
    "a36446dcaa3869d52087e046a23091da3bff725813c62d7efb1c3df1a144034200048e63289666cf",
    "24684c9a23004d84101d1a730f55636b24354f35ded6da33377a5e668cd8a8c454f29bafc6a63e",
    "64073af7119f009d11dc334d28aa326546cea3",
);

/// One issued test-only certificate/key pair.
pub(crate) struct Issued {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).expect("valid fixture hex");
            let low = (pair[1] as char).to_digit(16).expect("valid fixture hex");
            u8::try_from((high << 4) | low).expect("fixture byte fits u8")
        })
        .collect()
}

/// Returns the static test-only server (host) identity.
pub(crate) fn server_identity() -> Issued {
    Issued {
        cert_der: CertificateDer::from(decode_hex(SERVER_CERT_HEX)),
        key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(decode_hex(SERVER_KEY_HEX))),
    }
}

/// Returns the static test-only client identity.
pub(crate) fn client_identity() -> Issued {
    Issued {
        cert_der: CertificateDer::from(decode_hex(CLIENT_CERT_HEX)),
        key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(decode_hex(CLIENT_KEY_HEX))),
    }
}

/// Returns the shared rustls crypto provider (ring) used by every fixture.
pub(crate) fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Builds a server-side rustls config trusting only `trusted_client_cert`.
pub(crate) fn build_server_rustls_config(
    server: &Issued,
    trusted_client_cert: &CertificateDer<'static>,
) -> rustls::ServerConfig {
    let mut client_roots = RootCertStore::empty();
    client_roots
        .add(trusted_client_cert.clone())
        .expect("add trusted client root");
    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(client_roots))
        .build()
        .expect("build client cert verifier");

    let mut config = rustls::ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![server.cert_der.clone()], server.key_der.clone_key())
        .expect("server rustls config");
    config.alpn_protocols = vec![ALPN.to_vec()];
    config
}

/// Builds a client-side rustls config trusting only `trusted_server_cert`.
pub(crate) fn build_client_rustls_config(
    client: &Issued,
    trusted_server_cert: &CertificateDer<'static>,
) -> rustls::ClientConfig {
    let mut server_roots = RootCertStore::empty();
    server_roots
        .add(trusted_server_cert.clone())
        .expect("add trusted server root");

    let mut config = rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(server_roots)
        .with_client_auth_cert(vec![client.cert_der.clone()], client.key_der.clone_key())
        .expect("client rustls config");
    config.alpn_protocols = vec![ALPN.to_vec()];
    config
}

/// Wraps a server rustls config into a Quinn server config using the
/// crate's `recommended_transport_config_arc`.
pub(crate) fn build_quinn_server_config(
    rustls_config: rustls::ServerConfig,
    policy: &BoundedTransportPolicy,
) -> quinn::ServerConfig {
    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config)
        .expect("quic server crypto config");
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    config.transport_config(recommended_transport_config_arc(policy));
    config
}

/// Wraps a client rustls config into a Quinn client config using the
/// crate's `recommended_transport_config_arc`.
pub(crate) fn build_quinn_client_config(
    rustls_config: rustls::ClientConfig,
    policy: &BoundedTransportPolicy,
) -> quinn::ClientConfig {
    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config)
        .expect("quic client crypto config");
    let mut config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    config.transport_config(recommended_transport_config_arc(policy));
    config
}

/// Wraps a server rustls config into a Quinn server config using the
/// crate's test-only `monitor_carrier_transport_config_arc` (raised
/// concurrent unidirectional stream limit). Never used by product code;
/// only the monitor-stream loopback tests that genuinely need more than one
/// concurrent server-to-Deck monitor stream should call this instead of
/// `build_quinn_server_config`.
pub(crate) fn build_quinn_server_config_for_monitor_carrier(
    rustls_config: rustls::ServerConfig,
    policy: &BoundedTransportPolicy,
) -> quinn::ServerConfig {
    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config)
        .expect("quic server crypto config");
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    config.transport_config(monitor_carrier_transport_config_arc(policy));
    config
}

/// Wraps a client rustls config into a Quinn client config using the
/// crate's test-only `monitor_carrier_transport_config_arc` (raised
/// concurrent unidirectional stream limit). Never used by product code; see
/// `build_quinn_server_config_for_monitor_carrier`.
pub(crate) fn build_quinn_client_config_for_monitor_carrier(
    rustls_config: rustls::ClientConfig,
    policy: &BoundedTransportPolicy,
) -> quinn::ClientConfig {
    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config)
        .expect("quic client crypto config");
    let mut config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    config.transport_config(monitor_carrier_transport_config_arc(policy));
    config
}
