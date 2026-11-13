# shared/protocol — `arcen-protocol`

**Delivery:** the SHARED wire contract. Not a product surface; the single source of truth
every Pier and the Deck speak. Internal crate.

## What this is

The host-agnostic, serde-based on-wire protocol: message types, the auth handshake, the
connection state machine, bounded clipboard reassembly, lane prefaces, cursor records and
byte-level wire framing. Every Arcen Pier (Linux, Windows, macOS) and the Deck depend on it;
it depends on nothing else in-repo. It replaced an earlier Python message module, whose
JSON shapes it keeps.

## How it's grounded

- Lessons encoded:
  - **One wire, every platform.** The contract is deliberately host-agnostic so a Linux,
    Windows or macOS Pier is byte-interchangeable to the Deck. Do not add
    platform-specific fields; platform capability travels as negotiated fields, not forks.
  - **`ServerHello`/handshake carries capabilities + a version field** so a peer
    can negotiate without understanding the payload.
  - New fields are additive and `serde(default)`, gated by a capability, so an older Deck
    or Pier keeps working; `WIRE.md` records each with the release that introduced it.

## Rules — what it must be (invariants)

1. **Pure and safe.** `#![forbid(unsafe_code)]` at crate level. No FFI, no OS calls, no I/O
   beyond serde. It must cross-compile trivially for every target.
2. **No in-repo dependencies.** Depends only on approved third-party crates (serde,
   serde_json, sha2, getrandom). Never on hosts, clients, or packaging.
3. **Byte-stable.** Changing the wire is a breaking change: bump the protocol version and
   keep a compatibility fixture. Never silently reorder/rename serialized fields. Pre-1.0
   Rust-only API hardening (validated constructors, private fields, `#[non_exhaustive]`)
   is allowed without a wire bump, but still counts as a crate-API break to call out.
4. **Deterministic.** No wall-clock or RNG in serialization paths except where the protocol
   explicitly requires a nonce (auth).

## Interfaces / boundaries

- **Exposes:** message enums/structs, `auth` handshake helpers, the connection `fsm`, and
  `wire` encode/decode. Consumed by `arcen-deck-macos` (and its USB helper),
  `arcen-pier-linux`, `arcen-pier-windows`, `arcen-pier-macos`, `arcen-capenc`, and the
  shared `arcen-input`, `arcen-media` and `arcen-transport` crates.
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
- `lanes.rs` — QUIC lane roles and the bounded lane preface (lane protocol v1, ADR 0014).
- `cursor.rs` — the `CursorUpdate` binary record (cursor images) and its lane relay.
- `keys.rs` — the key identifiers and modifier bits a `key_event` carries beyond letters
  and digits (AltGr, the ISO key and the other extended keys).
- `build_identity.rs` — the build identity every product advertises in its hello.
- `WIRE.md` — the human-readable wire spec (migrate alongside).

## Deferred / roadmap

- Recorded session transcripts (a Linux, Windows and macOS Pier against the Deck) as
  decode-compatibility fixtures. None are committed; record them from a live capture,
  never reconstruct them.

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
