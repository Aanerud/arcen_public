//! Real Quinn loopback test for the host side of the audio priority stream:
//! `PriorityAudio` opens the stream on the first frame, a Deck reading it
//! receives the same bytes in order, and a closed connection makes it
//! unusable so audio falls back to the session stream.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use arcen_transport::BoundedTransportPolicy;
use arcen_transport::quic::{
    PriorityAudio, PriorityAudioSend, accept_audio_priority_stream, read_priority_frame,
};
use support::{
    build_client_rustls_config, build_quinn_client_config, build_quinn_server_config,
    build_server_rustls_config, client_identity, server_identity,
};

const TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_moves_audio_to_the_priority_stream_and_falls_back_when_it_fails() {
    let policy = BoundedTransportPolicy::default();
    let server = server_identity();
    let client = client_identity();
    let server_endpoint = quinn::Endpoint::server(
        build_quinn_server_config(
            build_server_rustls_config(&server, &client.cert_der),
            &policy,
        ),
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
    )
    .unwrap();
    let address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let accept = {
        let endpoint = server_endpoint.clone();
        tokio::spawn(async move { endpoint.accept().await.unwrap().await.unwrap() })
    };
    let deck = client_endpoint
        .connect_with(
            build_quinn_client_config(
                build_client_rustls_config(&client, &server.cert_der),
                &policy,
            ),
            address,
            "localhost",
        )
        .unwrap()
        .await
        .unwrap();
    let host = accept.await.unwrap();

    let mut priority = PriorityAudio::new(host.clone());
    assert!(priority.usable());
    let first = [0x10_u8, 1, 2, 3];
    let second = [0x10_u8, 4, 5];
    assert_eq!(priority.send(&first).await, PriorityAudioSend::Opened);
    assert_eq!(priority.send(&second).await, PriorityAudioSend::Sent);

    let mut recv = tokio::time::timeout(TIMEOUT, accept_audio_priority_stream(&deck))
        .await
        .expect("accepted in time")
        .expect("the Deck accepts the stream");
    assert_eq!(
        read_priority_frame(&mut recv).await.unwrap(),
        Some(first.to_vec())
    );
    assert_eq!(
        read_priority_frame(&mut recv).await.unwrap(),
        Some(second.to_vec())
    );

    priority.finish();
    assert_eq!(
        read_priority_frame(&mut recv).await.unwrap(),
        None,
        "finished cleanly"
    );

    host.close(0u32.into(), b"gone");
    let mut after = PriorityAudio::new(host);
    let outcome = after.send(&first).await;
    assert!(!outcome.delivered(), "{outcome:?}");
    assert!(!after.usable());
    assert_eq!(after.send(&first).await, PriorityAudioSend::Unusable);
}
