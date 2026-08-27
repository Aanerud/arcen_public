# ADR 0004: Platform Scope & Sequence

**Status:** Accepted; active-scope metadata updated 2026-07-31.

## Decision

The end goal is a full grid of native hosts and clients, then the gateway. Sequence:

1. **Hosts golden (current):** **Linux Pier** + **Windows Pier**, streaming to the **macOS
   Deck**, over **direct (machine-to-machine) connections** on a trusted network. Get this
   solid and shippable before widening.
2. **macOS Pier** (host) — after the two hosts are golden.
3. **Clients:** **Linux Deck**, **Windows Deck**, **macOS Deck** — scale the thin client to
   every platform, each optimized for its decode/render path.
4. **Arcen Span gateway** (Rust + QUIC, federated identity + MFA) — last, once hosts and
   clients are solid.

## Consequences

The workspace `members` carry the active shared surfaces (`identity`, `input`,
`media`, `protocol`, `session`, `keel`, `telemetry`, `observability`, and
`transport`), the macOS Deck, the two Piers, capenc + helpers, and the Windows
CP. `arcen-observability` is the OS-free tracing and bounded-I/O runtime for
this direct Pier/Deck scope, while `arcen-telemetry` remains pure.
`arcen-transport` is active for the dependency-light TLS lifecycle and direct
QUIC product carrier. Its `wss-compat` feature is dormant and non-shipping.
macOS host, Linux/Windows clients, and the gateway
are dormant on disk / out of `members` until their milestone. Shared APIs must avoid
unnecessary platform assumptions so deferred products slot in later without promising support
now. Direct connections are the trusted-LAN path; the gateway is where internet exposure +
enterprise identity live — the two are deliberately separate.
