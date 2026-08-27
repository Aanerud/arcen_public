# ADR 0005: Process-Local Direct-Transport Resume Authority

**Status:** Proposed; implemented in the current draft branch, pending required
architecture, security, and product-owner review.

## Context

ADR 0003 defines durable atomic replay consumption shared by multiple future
Gateway instances. The current product is different: one macOS Deck connects
directly to one Windows or Linux Pier over QUIC, each Pier admits one active
session, and no Gateway or shared store is live.

Using ADR 0003's durable multi-Gateway replay design here would introduce a
database and cross-process availability boundary without enabling a supported
topology. Reusing OIDC, entitlement, TLS session resumption, or a password as a
resume authority would also collapse separate trust planes.

## Decision

For direct QUIC, each Pier process creates a fresh system-random 256-bit key
at startup and owns it in zeroizing process memory. It signs and verifies with
ring HMAC-SHA-256 over `arcen-identity` canonical, length-delimited bytes whose
exact domain separator is:

```text
arcen-direct-quic-resume-v1
```

The target host identity is the stable validated TLS SPKI identity
`spki-sha256:<hash>`, not DNS, IP, or peer address. Claims also bind the active
host session, native principal/session, Deck holder nonce, disclaimer
digest/version, generation, nonce, issue time, and expiry. Product adapters
retain and compare the authenticated client/display/time-zone topology.

One mutex-protected slot contains the current generation+nonce. Validation and
constant-time compare-and-rotate occur before the replacement socket is handed
to the session owner. Only one concurrent attempt can consume a generation,
and every successful resume issues the next generation. Invalid, stale,
duplicated, malformed, or lost-response tokens never fall back to PAM,
password, or Credential Provider.

Validation has two stages under that mutex: authenticate structure, HMAC,
immutable/current session bindings, and time first; then compare generation and
nonce with the slot. A valid current candidate rotates normally. Only an
authenticated, unexpired exact predecessor presented while the slot is detached
(`candidate_generation + 1 == current_generation`) proves that Deck may have
lost the sole successor, so it atomically revokes the slot, notifies the owner,
and enters final drain. Older, ahead, wrongly signed, or incorrectly bound
candidates cannot trigger drain.

An opted-in attached session refreshes that same single slot every `W/2`
(millisecond precision for odd and one-second windows). The host rotates first
and sends the successor as a successful protocol-v3 `AuthResult` streaming
control update; there is no second acceptable slot and no acknowledgment.
Claims expire after `2W`, bounded by 14,400 seconds. Deck stores no connected
deadline: both Deck and host start their exact monotonic `W` detach deadline at
unexpected transport loss.

There is no database. A Pier restart loses both key and slot, so every old grant
fails closed and the user must perform normal authentication. This is
intentional for the one-process, one-session direct topology.

## Consequences

- Resume authority cannot survive a process crash, restart, host move, or
  certificate SPKI rekey.
- No cross-process synchronization, durable replay recovery, key distribution,
  or cross-host resume is claimed.
- Rotation before success delivery prevents replay but means a lost successful
  response also loses the only successor token. A known delivery failure drains
  immediately; otherwise the authenticated exact predecessor on a detached slot triggers that
  drain on its next presentation. Cleanup then permits visible normal
  authentication without reopening the predecessor or revealing the successor.
- The HMAC key and opaque grants are secrets: they are not persisted, formatted,
  logged, included in support bundles, or derived from TLS/password material.
- Attached refresh keeps one bounded `2W` claim current without extending the
  authoritative reconnect duration. A lost refresh leaves Deck's old token a
  replay after loss; generation/signing/delivery failure and authenticated
  exact-predecessor presentation drain terminally. Clock and arithmetic
  failures fail closed.

If Arcen supports resume across Pier processes, process restart, host migration,
multiple hosts, or Arcen Span/Gateway instances, this ADR must be replaced or
amended after review. That review must define durable atomic replay,
key rotation/distribution, cross-host identity, availability, revocation, and
data-store threat boundaries consistent with ADR 0003.

This decision does **not** activate or make claims about Gateway/Span, OIDC,
entitlement/licensing, mTLS, QUIC, QUIC 0-RTT, TLS 0-RTT, or internet exposure.
It is not a durable version of ADR 0003 and is not a certification or external
cryptographic audit.
