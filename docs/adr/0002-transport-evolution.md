# ADR 0002: Transport Evolution

**Status:** Superseded for product selection by
[ADR 0007](0007-quic-only-product-transport.md). This remains the historical
record of the staged WSS-to-QUIC migration and the richer stream/datagram
design. Revised 2026-07-31.

## Decision

The migration initially kept secure WebSocket (WSS) as the compatibility
default and added an explicitly selected direct QUIC profile using Quinn/rustls
TLS 1.3 and one bidirectional stream carrying the same authenticated
WebSocket-framed application protocol. That staged choice established the
measurable baseline used to accept ADR 0007. It is no longer release guidance:
shipped products now compile only the QUIC network path.

Do not silently downgrade QUIC. The accepted socket is transport truth: the
host reports it in `ServerHello`, and the client confirms it in `ClientHello`.
Standardized independent QUIC streams and
encrypted datagrams remain behind the same opt-in feature for a future,
separately reviewed optimization; the direct profile does not use them yet.
Arcen Span remains roadmap, not a live implication of direct QUIC. The shared
crate retains a dormant WSS compatibility abstraction behind `wss-compat`
without placing it in product builds. Both profiles are represented by the same product-neutral
`arcen-transport` contract types (`TransportProfile`, `ReliabilityClass`,
`DeliveryMechanism`, `BoundedTransportPolicy`, `TransportEvent`,
`TransportContractError`) so Host, Client, and Gateway components reason about
delivery semantics identically regardless of which concrete transport is
active. No vendor-specific QUIC wire format is exposed as the Arcen protocol;
QUIC is used only as a standardized transport underneath `arcen-protocol`.

### Direct compatibility profile

`arcen_transport::quic::DirectQuicStream` retains Quinn endpoint/connection
lifetime and implements Tokio `AsyncRead`/`AsyncWrite`. Pier accepts the first
bidirectional stream only after QUIC TLS/ALPN succeeds, then enters the same
bounded pre-authentication and product session loop as WSS. TLS establishment
is not application authorization. Deck reuses its existing trust modes and
never sends credentials before the transport and certificate checks complete.

The wire profile intentionally retains WebSocket framing over QUIC. This adds
small framing/masking overhead and keeps all application classes ordered behind
one stream, but avoids a simultaneous protocol rewrite. Multi-stream and
datagram work requires evidence from matched WSS/QUIC measurements.

### Admission and bounded receive

Every adapter applies per-class payload caps and total queue message/byte caps
before sending. Receive is metadata-first: message class, reliability/delivery
class, declared size, sequence, active session, and peer evidence are validated
before payload allocation. Adapters then fill an exact bounded allocation; a
bare allocation-returning receive method is not part of the shared contract.

Every connection retains authenticated peer role and identity, validated
session-grant identity/binding, negotiated capabilities, and explicit
authorization state. Active-session identifier equality alone never authorizes
relay or direct admission. Reconnect/path events remain observable independently
from protocol messages and replacement connections must present fresh admission
evidence.

WSS adapters must parse a bounded metadata header without allocating the
declared payload, enforce the shared acceptance token before reading payload
bytes, and reject malformed class combinations, oversized declarations,
unexpected classes, and mismatched sequence/session/peer evidence.

### Library selection and Rust compatibility

The QUIC adapter (`arcen_transport::quic`) is built on:

- **Quinn `0.11.11`** (pinned, `default-features = false`, features
  `runtime-tokio`, `rustls-ring`) for the QUIC protocol implementation and
  async I/O integration with Tokio.
- **rustls `0.23.41`** (pinned, `default-features = false`, features `ring`,
  `std`) for QUIC transport-layer TLS 1.3 cryptography. The dormant
  `wss-compat` feature adds rustls `tls12` only to compatibility builds. This is the
  *only* cryptography this crate uses. There is no custom UDP encryption, no
  bespoke handshake cipher, and no vendor keying scheme; all confidentiality,
  integrity, and key exchange are exactly what Quinn/rustls implement for
  QUIC per RFC 9000/9001.
- Quinn `0.11.11` declares Rust `1.85` as its MSRV, exactly matching the
  workspace. rustls `0.23.41` declares Rust `1.71`, so it remains compatible
  with that floor. The adapter is edition 2024 and uses no newer language API.

Each QUIC caller constructs and owns the actual
`quinn::ServerConfig` / `quinn::ClientConfig` — and therefore every rustls
certificate, private key, ALPN protocol list, and certificate verifier
decision. `arcen-transport` never bundles a default rustls verifier and never
ships an insecure "skip verification" helper. It only exposes
`quic::recommended_transport_config` / `recommended_transport_config_arc`, a
helper that fills in the stream/datagram limits this adapter needs (at least
one concurrent bidirectional stream for the binding handshake, at least one
concurrent unidirectional stream per direction for the persistent reliable
stream, and a bounded datagram buffer sized from the datagram payload cap);
callers attach it to their own configuration.

### Advanced-adapter stream and datagram mapping

`arcen-transport` enforces one fixed delivery-class mapping, independent of
profile:

| `ReliabilityClass`  | Allowed `DeliveryMechanism`                        |
| ------------------- | --------------------------------------------------- |
| `Control`           | `ReliableStream` only                                |
| `MediaReliable`      | `ReliableStream` only                                |
| `MediaLowLatency`    | `ReliableStream` or `EncryptedDatagram`               |
| `InputLowLatency`    | `ReliableStream` only (reliable unless a future reviewed profile changes it) |

`EncryptedDatagram` is additionally only available under the `Quic` profile;
`BoundedTransportPolicy::validate` rejects it under `WebSocketSecure` with
`DeliveryNotAvailableForProfile`, and rejects any non-`MediaLowLatency` class
attempting it with `DeliveryClassMismatch`.

Concretely, the QUIC adapter uses:

- **One persistent unidirectional stream per direction** (each side opens its
  own outbound `quinn::SendStream` and accepts the peer's inbound
  `quinn::RecvStream`), multiplexing framed `Control` / `MediaReliable` /
  `MediaLowLatency`-as-stream / `InputLowLatency` messages. Each fixed header
  carries semantic message class, reliability class, delivery class, declared
  size, and per-peer sequence. Session and peer identity evidence come from
  the non-cloneable authenticated connection binding and are checked with the
  header before the exact payload allocation. This preserves ordered,
  reliable delivery for the lifetime of the connection.
- **QUIC datagrams (RFC 9221)** exclusively for `MediaLowLatency` traffic,
  framed with the same class/delivery/declared-size evidence and an 8-byte
  sender sequence number. The decoder borrows the payload until metadata,
  capability, size, and duplicate/lateness checks succeed.
  Payloads are capped by both a conservative, configurable
  `BoundedTransportPolicy::max_datagram_payload_bytes` (default 1024 bytes)
  and the connection's live, path-derived `Connection::max_datagram_size()`;
  a datagram exceeding either is never sent, and receipt of a duplicate or
  too-late sequence number (per `quic::DatagramSequenceGuard`) is rejected
  rather than silently accepted out of order.

Opening a persistent uni stream is a purely local action (it only consumes
already-known peer stream-count budget) and must not be awaited alongside
accepting the peer's corresponding stream, since a QUIC stream is not
announced to the peer until data is actually written to it; the adapter
accepts each side's incoming persistent stream lazily, from within its own
background reader task, precisely to avoid that mutual-wait deadlock.

### Advanced-adapter admission, capability handshake, and TLS identity hooks

Before any application payload flows (reliable stream or datagram), both
peers provide a non-cloneable `QuicAdmission` containing a fresh atomically
consumed grant, local and expected remote authenticated identities, explicit
authorization, advertised capabilities, policy-required capabilities, and the
admission time. A replacement connection cannot reuse this value.

Handshake protocol v2 performs one bounded request/response exchange over a
dedicated QUIC bidirectional stream (`quic::handshake`). The initiator states
its claimed product-neutral role (`QuicRole::Host | Client | Gateway`), grant-
bound session, supported capabilities, and required capabilities. The acceptor
checks the exact expected role/session, computes the bounded capability
intersection, rejects missing profile or policy requirements, and acknowledges
its own role plus the selected capabilities. Rejections are explicit
(`HandshakeRejectReason::{Malformed, ProtocolVersionMismatch, Unauthorized,
SessionMismatch, CapabilityMismatch}`). Authorization is **mutual**: the initiator, in turn,
authorizes the acceptor's TLS identity against the role/session it just
acknowledged. Both directions require a caller-supplied
`quic::PeerIdentityAuthorizer` that inspects the peer's TLS certificate chain
(`Connection::peer_identity()` downcast to
`Vec<rustls::pki_types::CertificateDer<'static>>`) together with the claimed
role/session and grant-bound expected peer identity, then returns an explicit
accept/reject decision. This crate ships
no permissive default authorizer; callers must always supply one. Certificate
issuance, rotation, and revocation remain entirely a caller/product concern —
this hook only authorizes an already-TLS-validated chain against Arcen-level
role/session expectations, on top of whatever certificate verifier the
caller's rustls config already performs. The resulting `SessionBinding`
retains authenticated peer evidence, consumed-grant binding evidence,
negotiated capabilities, and explicit authorization; session identifier
equality by itself is never admission.

QUIC admission requires the `transport:quic-v1` and
`delivery:reliable-stream-v1` capabilities. Datagram delivery additionally
requires `delivery:encrypted-datagram-v1`. The transport-neutral profile selector requires QUIC in product builds.
Dormant `wss-compat` builds may exercise historical selection behavior, but
packaging and release builds must never enable it.

### Migration, MTU, and feedback

Quinn manages QUIC connection migration internally; this crate does not
claim, request, or control path migration. It only *observes* it: a
background lifecycle task polls `Connection::remote_address()` and
`Connection::stats().path.current_mtu` on a fixed interval and emits
`TransportEvent::PathChanged` / `TransportEvent::MtuChanged` when they change,
alongside a periodic `TransportEvent::Feedback` snapshot
(`quic::FeedbackSnapshot`) built from `Connection::stats().path`: RTT,
congestion window, congestion events, lost/sent packets and bytes, current
MTU, and black-hole detections. `QuicPeer::feedback_snapshot()` also exposes
an on-demand, synchronous snapshot independent of the ticker.

### Queues, caps, and reconnect

Outbound reliable-stream sends are validated against
`BoundedTransportPolicy` and enqueued into a bounded queue
(`max_queued_messages` / `max_queued_bytes`); a full queue returns an explicit
`OutboundQueueFull` error rather than silently dropping control/reliable
traffic. A send completes after Quinn's local stream writer accepts the frame;
it is not a remote application acknowledgement. Inbound reliable-stream data applies real backpressure through bounded message
and byte budgets instead of ever discarding reliable bytes. Header admission
and byte-budget acquisition both occur before payload allocation.
`recv_message` returns an `InboundEnvelope` retaining all validated metadata.
Encrypted datagrams are sent immediately and may
be dropped under path/buffer pressure, but every drop increments a counter
and emits an explicit `TransportEvent::DatagramDropped` with a specific
reason — never a silent failure.

Binding plus required stream establishment has a configurable finite deadline,
preventing a peer that never completes setup from occupying an adapter task
indefinitely. Event delivery is also bounded: periodic feedback/path samples
may be dropped for a slow consumer and are counted by
`QuicPeerCounters::events_dropped`; events are an operational surface, not a
durable audit log.

There is no automatic transport-level reconnect. `quic::reconnect` is an
explicit helper: it marks the outgoing peer with `TransportEvent::Reconnecting`,
dials a fresh connection through the ordinary `quic::connect` path, and marks
the new peer with `TransportEvent::Reconnected(new_connection_id)`, so a
caller always has both the old and new `ConnectionId` in hand.
Its `QuicDialParams` must contain a fresh `QuicAdmission`, so reconnect cannot
clone or reuse consumed grant evidence.

### No 0-RTT, no adaptive FEC

This crate does not enable or implement 0-RTT connection establishment.
Forward error correction is represented only by the explicit,
non-adaptive `FecPolicy::{Unsupported, Disabled}` — there is no adaptive FEC
implementation, and this type must never be read as claiming one exists.

## Consequences

The default TLS lifecycle and direct QUIC carrier have shared and product
coverage. Dormant WSS coverage is feature-gated and non-shipping. The
separately unused advanced QUIC adapter has:
unit coverage for the contract, framing, handshake codec, and sequence guard
in `shared/transport/src`; a deterministic localhost integration test using
real mutual TLS (static test-only certificates, private `RootCertStore`s, and an
explicit certificate-checking authorizer — never a no-verification helper)
under `shared/transport/tests`; transport-neutral mapping/role/FEC/reconnect
coverage in `tests/conformance/transport.rs`; and a pure, deterministic
loss/reorder/duplication/lateness model (explicitly not a live kernel
impairment tool) in `tests/network/quic_impairment.rs`. Production soak
testing, comparative performance baselines, cross-platform/kernel-level
impairment validation, and full telemetry integration remain follow-up work
tracked in `docs/architecture/transport.md`. Direct product activation uses the
single-stream compatibility profile, not the advanced adapter.

Compatibility coverage in `tests/compatibility/transport_downgrade.rs` remains
for the explicit dormant feature. Product capability tests require QUIC and
cannot silently downgrade.
