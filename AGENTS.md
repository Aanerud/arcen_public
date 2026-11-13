# Arcen — read this first

Arcen streams a real desktop from a host to a thin client. A **Pier** is a host.
A **Deck** is a client. They speak QUIC over TLS 1.3, on UDP 18444, and nothing
else. It is free software under **AGPL-3.0-only** with the Arcen Store
Distribution Exception (`LICENSE-EXCEPTION.md`), owned by **TUTTIFRUTTI AS**.
Anyone may build and run it; TUTTIFRUTTI AS is the official distributor of
signed builds, cloud workstation images and paid support. There is no licence
enforcement in the product and never a feature gated on an official build.

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

The proof this works: of roughly 108,000 lines in the macOS Deck, about 18,000
are the decoder, the Metal presenter and the display, tablet and HID modules a
port would replace. A second client is mostly a decoder and a window.

---

## The map

[`STRUCTURE.md`](STRUCTURE.md) describes every top-level file and folder,
including the local-only ones git ignores.

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
| `hosts/linux/` | Linux Pier: Xorg and Wayland (GNOME/Mutter) desktops, PAM, capture supervision |
| `hosts/windows/` | Windows Pier: WTS sessions, credential provider, capture |
| `hosts/macos/` | macOS Pier (preview): Aqua/LoginWindow sessions, ScreenCaptureKit → VideoToolbox, input, virtual microphone |
| `hosts/capenc/` | Capture and encode: NvFBC/XShm/PipeWire/DXGI/WGC → explicit conversion → NVENC/MF/OpenH264 |
| `hosts/audiocap/`, `hosts/input-helper/` | Linux audio capture and input helpers |
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

| Contract | Linux Pier | Windows Pier | macOS Pier | macOS Deck |
| --- | --- | --- | --- | --- |
| Auto / Speed, 8-bit | NvFBC → CUDA → NVENC; preserve the device-to-device fast path | DDA after real-frame proof, otherwise WGC BGRA8 | ScreenCaptureKit `420v` → VideoToolbox, untouched | Ordinary SDR decode/presentation |
| Grading, 10-bit SDR | Depth-30 Xorg → XShm RGB10 → shared conversion (desktop codes untouched; a declared PQ desktop converted on the sRGB curve) → CUDA upload → NVENC; `srgb`/VUI 13 to Decks that claim `srgb_transfer`, `bt709` otherwise | WGC FP16 scRGB → shared sRGB re-encode (the desktop's own code values, `srgb`/VUI 13 to Decks that claim `srgb_transfer`; legacy BT.709 OETF otherwise) → matrix → NVENC P16 | SCK `xf44` in sRGB (desktop codes, unconverted) → VideoToolbox HEVC Main 4:4:4 10, `srgb` tag to Decks that claim `srgb_transfer` (BT.709 otherwise), SPS-proven | Native `xf44` → 10-bit Metal, EDR off |
| HDR | Xorg downgrades to Grading unless the operator declares a Rec.2100 PQ desktop (`video.desktop_encoding`, shared `resolve_desktop_plan`, which converts SDR sessions and refuses 8-bit ones). Wayland on Mutter ≥ 50 with a qualified `force-hdr` route: wide `RecordMonitor` 10-bit PQ DMA-BUF → readback → NVENC P16 BT.2020/PQ (Grading converts the PQ composition to BT.709 SDR in shared code; for Decks that claim `srgb_transfer` with the inverse of Mutter's 2.2-power SDR decode, stated `srgb`/VUI 13); Mutter < 50 has no HDR screen sharing and serves Auto | HDR EDID/topology and exact-target HDR proof → WGC FP16 scRGB → BT.2020/PQ → NVENC P16 | HDR virtual panel, headroom proof → SCK `xf44` PQ/BT.2020 → Core Image 203-nit white → VideoToolbox Main 4:4:4 10 | Same 10-bit Metal path, PQ/EDR on only when the host returns PQ |

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
| Where a pen or bridged tablet points, and the Deck's pen dot | `shared/input` `tablet_pointer` (samples, lead, mapping) and `host_pointer` (feed, reporter, Deck authority, mark); the record is `shared/protocol` `cursor_position`. Extend these; a Pier adds only an adapter that produces `PenSample`s |
| How a bridged USB tablet is admitted, tagged for a desktop, or read | `shared/usb-bridge`: `tablet_claim`/`bridged_device_guard` (admission), `bridge_seat` (udev rules, Xorg classes), `wacom_intuos_pen` (pen reader for known models), `exporter`/`hid_host` (the bridge itself) |
| A new log event | `shared/telemetry`, emitted through `shared/observability` |
| A product path that uses a shared crate, or a new dependency on one | a record in that crate's `shared/<crate>/consumers.json` |
| Calling a platform API to capture, encode, inject or authenticate | the relevant `hosts/` or `clients/` directory, and nothing more |

A platform module should read as a thin adapter: gather what the OS gives you,
hand it to a shared type, act on what comes back.

---

## Hard boundaries

- **All first-party source is `AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception`,
  copyright TUTTIFRUTTI AS.** Every new source file starts with the two-line
  SPDX header (`scripts/licensing/spdx-headers.py --write` adds it); files that
  cannot carry a comment are covered in `REUSE.toml`, and `reuse lint` must
  pass. Vendored or generated third-party material keeps its own licence in
  `REUSE.toml` — never relabel it. Do not introduce code under a licence that
  cannot combine into an AGPL-3.0 work, and do not add a GPL/LGPL/AGPL
  dependency without Release/Security review (`deny.toml`): its authors have
  not granted the store exception.
- **No personal names or accounts.** The only identities in this repository
  are TUTTIFRUTTI AS and "The Arcen Authors": not in headers, `Cargo.toml`
  `authors`, comments, docs, paths, URLs, examples or commit metadata. Use
  placeholders such as `/home/artist` and `alice@corp.example`. Contributors
  are credited only in `CONTRIBUTORS.md` and release notes, as they choose.
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

## "Unavailable" is a finding to research, not a conclusion

You are expected to search and research before you declare a capability
unavailable, and to keep working on it. Use web search, upstream repositories,
merge requests, mailing lists, release notes and maintainers' blogs as normal
tools. A probe on one lab machine shows what *that* stack does today; it does
not show what Arcen can do.

When something looks blocked:

1. **Research upstream first.** Find the current status in the relevant project
   (for example libei, Mutter, PipeWire, the NVIDIA driver, macOS SDKs), the
   version that adds it, and whether a released distribution ships it. Cite
   what you found (URL, version, date).
2. **Build to the capability, not to today's lab.** If the upstream interface
   exists or is landing, implement Arcen's side now behind capability
   negotiation: advertised when the host proves it, refused visibly when not.
   Test it against the upstream project's own test or demo server when the
   compositor or OS does not ship it yet.
3. **Look for another route.** A different API, protocol, or session design may
   reach the same result. Write down the routes you considered and why.
4. **Keep the work going.** Missing parity in one path is a work package, not a
   release note. Run it in parallel with release work; never park it
   silently.
5. **Report honestly.** "Unavailable" may appear in user-facing docs only with
   the evidence (what was tried, what upstream lacks, which version would add
   it) and a tracked follow-up. "Not implemented yet" and "not possible" are
   different statements; never write the second when you mean the first.

Reference projects remain learn-only: research them, never copy from them.

---

## Ownership

| Role | Paths |
| --- | --- |
| Shared/Architecture | `shared/`, `docs/architecture/`, `docs/adr/`, `tests/` (with Release/Security) |
| Linux Host | `hosts/linux/`, `hosts/capenc/`, `hosts/audiocap/`, `hosts/input-helper/`, `packaging/linux/` |
| Windows Host | `hosts/windows/`, `packaging/windows/` |
| macOS Pier | `hosts/macos/`, the Pier half of `packaging/macos/` |
| macOS Client | `clients/macos/`, `packaging/macos/` |
| Release/Security | `.github/`, `legal/`, `docs/operations/`, `docs/security/`, `packaging/` (with each platform), dependency and release policy |

Local detail is in `shared/AGENTS.md`, `hosts/linux/AGENTS.md` (which also
covers `capenc`, `audiocap` and `input-helper`), `hosts/windows/AGENTS.md`,
`hosts/macos/AGENTS.md`, `clients/macos/AGENTS.md`, `packaging/AGENTS.md`,
`docs/AGENTS.md` and `tests/AGENTS.md`. Read the one for the area you are
changing.

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
python3 scripts/check_shared_contracts.py
scripts/ci/check-publication-hygiene.sh
```

The shared crates are the strict-lint gate. Migrated platform crates still carry
warnings and are tightened over time.

**Changing a shared crate** also obliges you to protect its consumers, not only
its own tests: `python3 scripts/check_shared_contracts.py --impact --base main
--commands` lists every affected product path and the validation it needs
(see [`docs/architecture/shared-consumers.md`](docs/architecture/shared-consumers.md)).

**Dependency purity proofs.** `cargo tree --locked -p arcen-transport -e normal`
must show no `quinn`, `tokio`, `tokio-util` or `bytes` line, and
`cargo tree --locked -p arcen-media -e all` must show no `openh264`,
`openh264-sys2`, `wide`, `nasm-rs`, `cc` or `walkdir` line.

**On the target OS**, for platform work: macOS (`-p arcen-deck-macos`, and
`-p arcen-pier-macos` for the macOS Pier), Linux
(`-p arcen-pier-linux`, needs `libpam0g-dev` and `libpulse-dev`; release
builds add `--features wayland-provider`, which needs the PipeWire development
headers and libclang), Windows/MSVC
(`-p arcen-pier-windows -p arcen-capenc -p arcen-credential-provider`).

**CI does not build the platform crates.** It is manual (`workflow_dispatch`)
and Linux-only. Platform builds are yours to run, so say plainly what you built
and what you did not. "Built on Linux, ran the Pier tests, did not try Windows"
is a good description. Silence is not.

---

## Current state

The supported product surfaces are the Linux Pier, the Windows Pier, and the
macOS Deck. On X11 the Linux Pier provides a separate zero-copy eight-bit NvFBC
path and a genuine ten-bit XShm SDR path; Xorg HDR requests degrade to Grading.
Without Xorg it serves a qualified GNOME/Mutter Wayland desktop, headless or
native on its own seat, with Grading and HDR only on Mutter 50 or newer.
Windows provides separate eight-bit, FP16 Grading, and verified FP16 HDR paths.
Grading carries the desktop's own sRGB code values on every Pier and is
tagged `srgb` for Decks that present it (shared `serve_sdr_desktop`). The macOS Deck presents ten-bit
through a dedicated Metal layer and enables EDR only for PQ. The macOS Pier
ships as a signed preview package and serves a logged-in Aqua desktop to a
real Deck, but it is not a supported host release: cold-boot LoginWindow
operation, multi-monitor hardware qualification and Release/Security review
remain open, and it has no committed-text route or cursor images yet. A Linux Deck and a Windows Deck do not exist. `README.md` is the current status in detail.
