# shared/protocol — `arcen-protocol`

**Delivery:** the SHARED wire contract. Not a product surface; the single source of truth
every Pier and the Deck speak. Internal crate.

## What this is

The host-agnostic, serde-based on-wire protocol: message types, the auth handshake, the
connection state machine, bounded clipboard reassembly, and byte-level wire framing. Both Arcen Pier (Linux/Windows)
and Arcen Deck depend on it and on nothing else in-repo. Byte-compatible with the legacy
Python `common/messages.py`.

## How it's grounded

- Lessons encoded:
  - **One wire, three platforms.** The contract is deliberately host-agnostic so a Linux
    Pier and a Windows Pier are byte-interchangeable to the Deck. Do not add
    platform-specific fields; platform capability travels as negotiated fields, not forks.
  - **`ServerHello`/handshake carries capabilities + a version field** so a future Arcen
    Span gateway can proxy/negotiate without understanding the payload.
  - Compatibility is verified by transcript fixtures (see roadmap): a recorded
    Linux-host↔macOS-client and Windows-host↔macOS-client session must decode identically.

## Rules — what it must be (invariants)

1. **Pure and safe.** `#![forbid(unsafe_code)]` at crate level. No FFI, no OS calls, no I/O
   beyond serde. It must cross-compile trivially for every target.
2. **No in-repo dependencies.** Depends only on approved third-party crates (serde,
   serde_json, sha2, getrandom). Never on hosts, clients, gateway, or packaging.
3. **Byte-stable.** Changing the wire is a breaking change: bump the protocol version and
   keep a compatibility fixture. Never silently reorder/rename serialized fields. Pre-1.0
   Rust-only API hardening (validated constructors, private fields, `#[non_exhaustive]`)
   is allowed without a wire bump, but still counts as a crate-API break to call out.
4. **Deterministic.** No wall-clock or RNG in serialization paths except where the protocol
   explicitly requires a nonce (auth).

## Interfaces / boundaries

- **Exposes:** message enums/structs, `auth` handshake helpers, the connection `fsm`, and
  `wire` encode/decode. Consumed by `arcen-deck-macos`, `arcen-pier-linux`,
  `arcen-pier-windows`, and (indirectly, via framing) `arcen-capenc` output.
- **Consumes:** nothing in-repo.

## Module map

- `lib.rs` — crate root, re-exports, `#![forbid(unsafe_code)]`.
- `messages.rs` — all wire message types, including exact clipboard-v1
  policy/offers and negotiated input-v4 region-scoped input DTOs.
- `region_input.rs` — dependency-free typed region pointer/pen JSON DTOs. Products
  convert their primitive wire ids/coordinates to `arcen-media` domain types;
  this crate deliberately does not depend on `arcen-media` or `arcen-input`.
- `multi_monitor.rs` — bounded multi-monitor negotiation and applied topology,
  including the required per-monitor media roster (`stream_epoch`, backend,
  codec/chroma, encoded size, fps, bitrate, and cursor truth).
- `auth.rs` — credential handshake (challenge/nonce, hashing via sha2).
- `fsm.rs` — connection state machine (handshake → authed → streaming → teardown).
- `wire.rs` — byte framing: media headers and exact bounded clipboard chunks.
- `clipboard.rs` — one contiguous latest-wins clipboard reassembly with scrub/expiry.
- `WIRE.md` — the human-readable wire spec (migrate alongside).

## Deferred / roadmap

- Harvest SOL's `import-proven-protocol` v3 transcript fixtures
  (`linux_host_macos_client_v3.json`, `windows_host_macos_client_v3.json`) **only** if they
  validate byte-for-byte against the real wire; otherwise regenerate from a live capture.
- Add the capability-handshake version field for Arcen Span if not already present.

## Resume pointer

- **Status:** ✅ ACTIVE. Wire protocol v3 retains additive input-v2/v3,
  reconnect, and exact-negotiated clipboard-v1 surfaces. The independent input
  subprotocol is v4: Match My Layout requires mutual `region_input=available`
  and uses direct Region* pointer/pen DTOs, while non-region single-monitor
  compatibility may retain legacy desktop-coordinate input.
  `multi_monitor_v1` negotiation metadata is active for direct Deck/Pier
  sessions.
  The current shared contract requires an explicit pre-auth host
  offer before a client may send auth-time multi-monitor data, and carrier
  choice is derived from the auth-time host/client intersection before
  `ServerHello`; the later `ClientHello` sidecar remains echo/diagnostic only.
- **Original next step (done):** establish the five-module wire-contract crate
  plus `WIRE.md`, name the package `arcen-protocol`, set `path = "src/lib.rs"`,
  add `#![forbid(unsafe_code)]`, wire into the workspace,
  `cargo test -p arcen-protocol`.
