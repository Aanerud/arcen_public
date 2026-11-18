# ADR 0007: QUIC-Only Product Transport

**Status:** Accepted (2026-07-31) — implemented 2026-07-31; amended
2026-11-15 (the Decks' dormant WSS connector removed)

**Fleet validation:**
- pier-linux.example.internal (Linux): 8,881 frames decoded, keyboard + mouse + scroll, UDP 18444, RTT ~35 ms
- pier-windows.example.internal (Windows): 7,172 frames decoded, 161 s session, UDP 18444, `reason_class: user_quit`
- WSS wire markers absent from all shipped binaries (verified via `strings` inspection)

## Context

Arcen proved authenticated direct QUIC sessions end to end between macOS Deck
and both Linux and Windows Pier. Matched lab runs showed modest latency and
delivery improvements on some hosts, including the strongest operator
experience on pier-windows.example.internal, while also showing that presentation and audio
bottlenecks remain above the transport layer.

Arcen uses QUIC as its forward transport for its technical position and
future development. Shipping both WSS and QUIC would preserve
two network attack surfaces, two operational port models, a runtime selector,
and a fallback path that could quietly reverse that decision.

## Decision

- Shipped Arcen Deck and Arcen Pier binaries use direct QUIC with TLS 1.3 and
  ALPN `arcen-quic-v1` exclusively.
- Pier listens on UDP port `18444` by default. `listen.port` is the canonical
  configurable QUIC UDP port.
- QUIC configuration, certificate, resolution, or bind failure is fatal. There
  is no runtime fallback or downgrade.
- Product capability negotiation advertises and accepts only
  `transport:quic-v1`.
- Deck has no shipped transport selector. New and migrated saved connections
  resolve to QUIC; legacy port `18443` migrates to `18444`.
- The old `listen.quic_port` key remains a read-only migration alias for
  existing configurations. New templates and documentation must not emit it.
- The Decks have no WSS connector at all (amendment, 2026-11-15): the macOS
  Deck's and `arcen-deck-core`'s dormant `wss-compat` feature, the connector,
  the transport selector and the WSS choice on the Add connection screen are
  removed, and a connection saved as `"wss"` loads and is saved again as QUIC
  (port `18443` becomes `18444`; every other field is kept).
- The Piers' direct WSS listener, capability, TLS 1.2, and handshake support
  may remain in source only behind the default-off `wss-compat` Cargo feature
  (with the matching `arcen-protocol` and `arcen-transport` features).
  Packaging, CI product builds, and release automation must never enable that
  feature. *Amended 2026-11-18:* with both Decks QUIC-only, the Piers' dormant
  listener, the `wss-compat` features of both Piers, `arcen-protocol` and
  `arcen-transport`, the `transport:wss-v1` capability, the TLS 1.2 posture
  and the transport fallback policy are removed from source, and
  `tokio-rustls` (with `http`, `httparse`, `sha1`) leaves the lockfile.
  Tungstenite stays only as the in-process message type and lane framing
  codec.
- Installers open only UDP `18444` and remove the legacy TCP `18443` firewall
  rule when upgrading.
- CI verifies product feature graphs contain no `wss-compat`, TLS 1.2,
  Tokio-rustls listener, or Tungstenite handshake feature, and release binaries
  do not contain the WSS wire capability.

The current QUIC carrier intentionally retains bounded WebSocket *message
framing* over one reliable QUIC stream, and Windows broker/agent IPC uses the
same framing library. That internal codec is not a WSS network listener,
connector, TLS downgrade, or fallback. Replacing the framing is a separate
protocol optimization and is not required to enforce this transport boundary.

## Consequences

- QUIC is the only shipped direct network path and the only target for future
  transport tuning.
- Operators expose one inbound UDP port instead of parallel TCP and UDP ports.
- A QUIC regression fails visibly rather than silently downgrading to WSS.
- Dormant Pier WSS source can still be compiled deliberately for isolated
  migration work, but such binaries are not releasable. No Deck can connect
  to such a Pier over WSS.
- Existing WSS-specific ADR wording remains historical context only. This ADR
  supersedes the compatibility-default and fallback decisions in
  [ADR 0002](0002-transport-evolution.md) and generalizes the transport-specific
  naming in [ADR 0005](0005-direct-transport-resume-authority.md).
