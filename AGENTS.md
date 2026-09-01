# Arcen — read this first

Arcen streams a real desktop from a host to a thin client. A **Pier** is a host.
A **Deck** is a client. They speak QUIC over TLS 1.3, on UDP 18444, and nothing
else. It is free software under **AGPL-3.0-only** — no commercial edition, no
licence enforcement, no support commitment.

Everything is named `arcen-*`.

---

## The one rule

**Put it in `shared/` unless the operating system forbids it.**

Arcen exists three times over — Linux host, Windows host, macOS client — and it
stays maintainable only because the logic lives in one place and the platforms
own as little as possible. Every time something portable is written inside
`hosts/` or `clients/`, it has to be written again for the next platform, and
the two copies drift.

Before you add a file, ask one question:

> Could this compile and pass its tests on another operating system?

If yes, it belongs in `shared/`. If you are unsure, it belongs in `shared/`.

Platform directories are for the irreducible parts only: grabbing pixels,
driving a GPU encoder, injecting input, authenticating a user, putting a window
on a screen. Everything around those — negotiation, policy, state machines,
validation, framing, error taxonomy — is shared.

The proof this works: of roughly 70,000 lines in the macOS Deck, about 11,600
touch macOS at all. A second client is mostly a decoder and a window.

---

## The map

**`shared/` — eleven crates, no OS calls, compiled and tested everywhere.**

| Crate | Owns |
| --- | --- |
| `protocol` | The wire format. The single source of truth for what a Pier and a Deck say to each other. |
| `transport` | QUIC and TLS: certificate lifecycle, validation, pinning. |
| `media` | Codec negotiation, colour, clipboard rules, video plane maths, the OpenH264 software encoder. |
| `input` | Pointer, keyboard and pen: motion, ordering, cursor authority. |
| `outputs` | Monitor lifecycle and multi-display arrangement. |
| `session` | Reconnection, restore leases, deskside policy. |
| `identity` | Resume grants, acknowledgement evidence. |
| `keel` | Damage tracking — which 16×16 blocks changed. |
| `telemetry` | The event vocabulary. |
| `observability` | Structured logging that cannot block a video thread. |
| `usb-bridge` | USB passthrough policy and state. |

**Products — thin by design.**

| Path | Is |
| --- | --- |
| `hosts/linux/` | Linux Pier: Xorg session, PAM, capture supervision |
| `hosts/windows/` | Windows Pier: WTS sessions, credential provider, capture |
| `hosts/capenc/` | Capture and encode: NvFBC/XShm/DXGI/WGC → explicit conversion → NVENC/MF/OpenH264 |
| `clients/macos/` | macOS Deck: VideoToolbox decode, egui window |
| `packaging/` | Installers, signing, release metadata |

**Dependency direction is one way, and CI proves it:**

- Shared crates depend only on other shared crates or approved third-party
  crates. Never on `hosts/`, `clients/`, or `packaging/`.
- Hosts and clients depend on shared crates, never on each other.
- Shared crates stay light. `arcen-transport` must not pull a QUIC runtime into
  a program that only wanted certificate handling; `arcen-media` must not pull
  in a native codec build. CI inspects the dependency tree rather than trusting
  the author.

## Streaming pipeline boundary

Auto, Speed, Grading, and HDR are complete contracts, not independent switches
applied to one capture loop. Keep their capture providers separate:

| Contract | Linux Pier | Windows Pier | macOS Deck |
| --- | --- | --- | --- |
| Auto / Speed, 8-bit | NvFBC → CUDA → NVENC; preserve the device-to-device fast path | DDA after real-frame proof, otherwise WGC BGRA8 | Ordinary SDR decode/presentation |
| Grading, 10-bit SDR | Depth-30 Xorg → XShm RGB10 → shared conversion → CUDA upload → NVENC | WGC FP16 scRGB → shared SDR transfer/matrix → NVENC P16 | Native `xf44` → 10-bit Metal, EDR off |
| HDR | Xorg downgrades to Grading; only a future proven Wayland provider may retain PQ/HLG | HDR EDID/topology and exact-target HDR proof → WGC FP16 scRGB → BT.2020/PQ → NVENC P16 | Same 10-bit Metal path, PQ/EDR on only when the host returns PQ |

Do not widen or refactor the 8-bit fast path to implement a fidelity path.
Bit depth does not imply HDR. Capture source, transfer, primaries, matrix,
cursor capability, copy cost, and degradation must remain explicit in the
resolved plan, READY/hello truth, frame headers, and Deck presentation.

---

## Where a change goes

| You are adding | It goes in |
| --- | --- |
| A new message or field on the wire | `shared/protocol` |
| A codec, colour or clipboard decision | `shared/media` |
| How input is ordered, accumulated or attributed | `shared/input` |
| When a session may resume, and for how long | `shared/session` |
| A new log event | `shared/telemetry`, emitted through `shared/observability` |
| Calling a platform API to capture, encode, inject or authenticate | the relevant `hosts/` or `clients/` directory, and nothing more |

A platform module should read as a thin adapter: gather what the OS gives you,
hand it to a shared type, act on what comes back.

---

## Hard boundaries

- **All first-party source is AGPL-3.0-only.** Any new file inherits it. Do not
  introduce code under a licence that cannot combine into an AGPL-3.0 work.
- **Never import uncleared third-party source.** No proprietary payloads, no
  vendor SDK material, no decompiled binaries, no local reference corpus.
  Dependencies must be open source, and anything shipping in a release artefact
  needs its full notice in
  [`legal/THIRD_PARTY_NOTICES.md`](legal/THIRD_PARTY_NOTICES.md), which is
  exhaustive by policy.
- **Never commit secrets, credentials, private keys, or customer data.**
- **Never commit private infrastructure.** No internal hostnames, no private IP
  addresses, no SSH runbooks against real machines, no VPN profile names. Use
  RFC 5737 addresses (`203.0.113.x`) and `*.example.internal` placeholders. This
  repository is public; treat everything in it as published.
- Legal notices, dependency policy, CI trust boundaries and release metadata
  need Release/Security review.

`scripts/ci/check-publication-hygiene.sh` enforces most of this. Run it before
you propose a change.

---

## Ownership

| Role | Paths |
| --- | --- |
| Shared/Architecture | `shared/`, `docs/architecture/`, `docs/adr/` |
| Linux Host | `hosts/linux/`, `hosts/capenc/`, `hosts/audiocap/`, `hosts/input-helper/`, `packaging/linux/` |
| Windows Host | `hosts/windows/`, `packaging/windows/` |
| macOS Client | `clients/macos/`, `packaging/macos/` |
| Release/Security | `.github/`, `legal/`, `docs/operations/`, `docs/security/`, dependency and release policy |

Each of those directories has its own `AGENTS.md` with local detail. Read the
one for the area you are changing.

Escalate shared public API, protocol, transport, trust-boundary or dependency
changes to Shared/Architecture. Escalate authentication, privilege, secrets, CI
permissions, packaging or release changes to Release/Security.

---

## Validating a change

There is no single "build the workspace" command, on purpose: the client is
macOS-only, the Windows host is Windows-only, the Linux host is Linux-only.

**Anywhere** — the shared crates and the hygiene gate:

```sh
python3 -m unittest scripts/test_validate_observability.py
cargo fmt --all --check
cargo test --locked -p arcen-identity -p arcen-input -p arcen-keel \
  -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol \
  -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge
cargo clippy --locked -p arcen-identity -p arcen-input -p arcen-keel \
  -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol \
  -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge \
  -- -D warnings
cargo test --locked -p arcen-media --features software-h264-source
cargo test --locked -p arcen-transport --features quic
scripts/ci/check-publication-hygiene.sh
```

The shared crates are the strict-lint gate. Migrated platform crates still carry
warnings and are tightened over time.

**Dependency purity proofs.** `cargo tree --locked -p arcen-transport -e normal`
must show no `quinn`, `tokio`, `tokio-util` or `bytes` line, and
`cargo tree --locked -p arcen-media -e all` must show no `openh264`,
`openh264-sys2`, `wide`, `nasm-rs`, `cc` or `walkdir` line.

**On the target OS**, for platform work: macOS (`-p arcen-deck-macos`), Linux
(`-p arcen-pier-linux`, needs `libpam0g-dev` and `libpulse-dev`), Windows/MSVC
(`-p arcen-pier-windows -p arcen-capenc -p arcen-credential-provider`).

**CI does not build the platform crates.** It is manual (`workflow_dispatch`)
and Linux-only. Platform builds are yours to run, so say plainly what you built
and what you did not. "Built on Linux, ran the Pier tests, did not try Windows"
is a good description. Silence is not.

---

## Current state

All three product crates build and pass their tests on their target OS. Linux
provides a separate zero-copy eight-bit NvFBC path and genuine ten-bit XShm SDR
path; Xorg HDR requests degrade to Grading. Windows provides separate eight-bit,
FP16 Grading, and verified FP16 HDR paths. The macOS Deck presents ten-bit
through a dedicated Metal layer and enables EDR only for PQ. A macOS Pier, a
Linux Deck and a Windows Deck do not exist; the gateway is not shipped.
`README.md` is the current status in detail.
