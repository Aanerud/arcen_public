# Repository structure

What every top-level file and folder is, why it is here, and who owns it.
Owner roles come from the ownership table in [`AGENTS.md`](AGENTS.md); each
owned directory has its own `AGENTS.md` with local rules and validation.

The layout follows one rule: portable logic lives in `shared/`, and the
platform directories (`hosts/`, `clients/`) hold only what an operating system
forces to be platform-specific.

## Top level

### Folders

| Path | What it is and why it is here | Owner |
| --- | --- | --- |
| [`shared/`](shared) | Eleven Rust crates with no operating-system calls: the wire protocol, transport, media, input, sessions and the rest. Built and tested on every OS. | Shared/Architecture |
| [`hosts/`](hosts) | The Piers (hosts) and their helper processes. Only capture, encode, input injection, authentication and session plumbing that an OS forces to be native. | Linux Host, Windows Host, macOS Pier (per subfolder) |
| [`clients/`](clients) | The Decks (clients). Today only the macOS Deck. | macOS Client |
| [`packaging/`](packaging) | Installers, packages, service units, signing and release-input checks for each platform. | Release/Security, co-owned per platform |
| [`docs/`](docs) | Architecture, decisions (ADRs), operations, security and testing documentation. | Shared/Architecture and Release/Security |
| [`scripts/`](scripts) | CI gates, publication hygiene, shared-contract checks, vendored-source verification and lab harnesses. | Release/Security (CI and review), Shared/Architecture (contract checks), platform owners (lab) |
| [`tools/`](tools) | Two internal research and diagnosis tools that are not part of any product. | Shared/Architecture |
| [`tests/`](tests) | Cross-component test contracts and fixtures that do not belong to a single crate. | Shared/Architecture and Release/Security |
| [`third_party/`](third_party) | Vendored, patched open-source dependencies, each with its notice and a source manifest. | Release/Security |
| [`legal/`](legal) | Origin statement and the exhaustive third-party notices. | Release/Security |
| [`.github/`](.github) | GitHub configuration: the CI workflow, issue forms, the pull-request template and release-note categories. Not ignored on purpose: GitHub reads it from the repository. | Release/Security |
| [`dist/`](dist) | Where release binaries are assembled. Only `dist/README.md` is tracked; the installers, apps and `dist/releases/<version>/` are local build output and never committed. | Release/Security |

### Files

| Path | What it is and why it is here | Owner |
| --- | --- | --- |
| [`README.md`](README.md) | What Arcen is, what works today, install, connect, how it works, build and contribute. | Release/Security (release claims) |
| [`AGENTS.md`](AGENTS.md) | Rules for anyone changing the code: the `shared/` rule, the map, ownership and the validation gates. | Shared/Architecture |
| `STRUCTURE.md` | This file: a map of the repository layout. | Shared/Architecture |
| [`CHANGELOG.md`](CHANGELOG.md) | User-visible changes per release, with an `[Unreleased]` section for work in progress. | Release/Security |
| [`SECURITY.md`](SECURITY.md) | How to report a vulnerability. | Release/Security |
| [`SUPPORT.md`](SUPPORT.md) | Where to get help: community channels without a commitment, and paid support from TUTTIFRUTTI AS. | Release/Security |
| [`LICENSE`](LICENSE) | The GNU Affero General Public License v3.0, verbatim from gnu.org. All first-party source is `AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception`. | Release/Security |
| [`LICENSE-EXCEPTION.md`](LICENSE-EXCEPTION.md) | The Arcen Store Distribution Exception, an additional permission under AGPLv3 section 7. | Release/Security |
| [`LICENSES/`](LICENSES) | One text per licence identifier used in file headers and `REUSE.toml`, as the REUSE specification requires. | Release/Security |
| [`REUSE.toml`](REUSE.toml) | Licence and copyright for files that cannot carry a header (data, images, fixtures) and the true licences of vendored third-party code. `reuse lint` must pass. | Release/Security |
| [`NOTICE`](NOTICE) | Copyright notice and the official-distribution statement of TUTTIFRUTTI AS. | Release/Security |
| [`TRADEMARKS.md`](TRADEMARKS.md) | The trademark policy for the names Arcen, Arcen Deck and Arcen Pier and the logos. | Release/Security |
| [`CLA.md`](CLA.md) | The individual and entity Contributor License Agreements (licence grant, not assignment). | Release/Security |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to contribute, the CLA in plain language, and how credit works. | Release/Security |
| [`CONTRIBUTORS.md`](CONTRIBUTORS.md) | Everyone credited for work in Arcen, and notable forks and integrations. | Release/Security |
| [`.gitleaks.toml`](.gitleaks.toml) | Secret-scan rules used by CI and by the publication pipeline, extending gitleaks' defaults. | Release/Security |
| [`VERSION`](VERSION) | The product version used by the build and packaging scripts. | Release/Security |
| [`Cargo.toml`](Cargo.toml) | The Cargo workspace: members, the default members that build on any machine, shared package metadata and dependency versions. | Shared/Architecture (boundaries), Release/Security (dependencies) |
| [`Cargo.lock`](Cargo.lock) | Locked dependency versions; every gate builds with `--locked`. | Release/Security |
| [`rust-toolchain.toml`](rust-toolchain.toml) | Pins the Rust toolchain so every machine builds with the same compiler. | Release/Security |
| [`deny.toml`](deny.toml) | `cargo deny` policy: allowed licences (all must combine into AGPL-3.0-only), advisories and sources. | Release/Security |
| [`clippy.toml`](clippy.toml) | Workspace Clippy configuration, including methods that are disallowed in favour of supervised wrappers. | Shared/Architecture |
| [`.gitignore`](.gitignore) | Keeps build output, key material, support bundles and local working material out of git. | Release/Security |
| [`.gitattributes`](.gitattributes) | Line-ending and binary rules where they matter (vendored Opus source, Windows driver INF files, JSON Lines). | Release/Security |

### Local-only folders (gitignored, never published)

These exist in a developer's checkout but git ignores them, and
`scripts/sync-to-build-host.sh` copies only the git source allowlist, never
them. Nothing in them is ever committed or published.

| Path | What it is |
| --- | --- |
| `target/` | Cargo build output. Rebuildable; safe to delete. |
| `dist/` (everything except `README.md`) | Locally built installers and apps, and the write-once `dist/releases/<version>/` sets attached to GitHub releases. |
| `.local/` | Local scratch output from scripts (reports, temporary trees). |
| `support/` | Support bundles copied back from hosts. They hold logs, configuration and host identity. |
| `keys/` | Local key material, if present. |

## Second level

### `shared/` — portable crates (Shared/Architecture)

| Crate | Owns |
| --- | --- |
| `protocol` (`arcen-protocol`) | The wire format; `WIRE.md` documents every message. |
| `transport` (`arcen-transport`) | QUIC and TLS: certificate lifecycle, validation, pinning. The QUIC runtime is an optional feature. |
| `media` (`arcen-media`) | Codec negotiation, colour, clipboard rules, video plane maths; the OpenH264 software encoder behind a feature. |
| `input` (`arcen-input`) | Pointer, keyboard and pen: motion, ordering, keyboard policy, cursor authority. |
| `outputs` (`arcen-outputs`) | Monitor lifecycle, multi-display arrangement and admission. |
| `session` (`arcen-session`) | Reconnection, restore leases, deskside policy. |
| `identity` (`arcen-identity`) | Resume grants and acknowledgement evidence. |
| `keel` (`arcen-keel`) | Damage tracking: which 16×16 blocks changed. |
| `telemetry` (`arcen-telemetry`) | The event vocabulary. |
| `observability` (`arcen-observability`) | Structured logging that cannot block a video thread. |
| `usb-bridge` (`arcen-usb-bridge`) | USB passthrough policy and state machines. |

Each crate lists its product consumers in `consumers.json`
(see [`docs/architecture/shared-consumers.md`](docs/architecture/shared-consumers.md)).

### `hosts/` — the Piers

| Path | Is | Owner |
| --- | --- | --- |
| `linux/` | Linux Pier (`arcen-pier-linux`): QUIC control plane, PAM, X11 and Wayland desktops, capture supervision. | Linux Host |
| `capenc/` | Capture-and-encode process spawned by the Linux and Windows Piers: NvFBC/XShm/DXGI/WGC capture, explicit conversion, NVENC/Media Foundation/OpenH264 encode. | Linux Host |
| `audiocap/` | Linux audio capture helper: PulseAudio/PipeWire monitor to 48 kHz stereo PCM. | Linux Host |
| `input-helper/` | Linux input helper; its library is linked into the single `arcen-pier` binary as a subcommand. | Linux Host |
| `windows/` | Windows Pier (`arcen-pier-windows`) plus `credential-provider/`, `cp-ipc/` (its sealed pipe contract), `iddcx-provider/` (virtual display planning), `driver/` (virtual display and microphone drivers), `data/` and `scripts/`. | Windows Host |
| `macos/` | macOS Pier (`arcen-pier-macos`, preview) plus `microphone-driver/` (virtual microphone AudioServerPlugIn). | macOS Pier (see `hosts/macos/AGENTS.md`) |

### `clients/` — the Decks

| Path | Is | Owner |
| --- | --- | --- |
| `macos/` | macOS Deck (`arcen-deck-macos`): VideoToolbox decode, Metal/egui window, input capture; `usb-helper/` is the privileged USB capture helper; `usb-iousbhost/` is its IOUSBHost capture backend (ADR 0019, not yet used by release builds); `secure-dir/` verifies and pins the root-only directories the helper keeps state in; `SETTINGS.md` documents every setting. | macOS Client |

### `packaging/` — installers and release inputs

| Path | Is | Owner |
| --- | --- | --- |
| `common/` | Staging, promotion and checksum scripts shared by all platforms. | Release/Security |
| `linux/` | Single-file Linux installer crate, systemd unit, default `pier.json`, logrotate, Xorg config, SELinux policy, certificate and deploy scripts, and `tools/gnome-prefix/` (lab-only GNOME build recipes). | Linux Host, Release/Security |
| `macos/` | Deck and Pier app/package build scripts, entitlements, Info.plist, PAM file, icon, release-input validation and tests. | macOS Client, Release/Security |
| `windows/` | Windows installer crate, default `pier.json`, event-log source, package and driver-source checks, MSVC wrapper; `client/` is a dormant scaffold for a future Windows Deck. | Windows Host, Release/Security |
| `quic_config_migration.rs` | Shared installer code that migrates older Pier configuration to the QUIC product port, included by the Linux installer. | Release/Security |

### `docs/` — documentation (Shared/Architecture and Release/Security)

| Path | Holds |
| --- | --- |
| `user-guide.md` | The Deck user guide for artists and editors, task by task. Each section names the source files it was checked against. |
| `architecture/` | How each part works and why, including the Wayland qualification records. |
| `adr/` | Architecture decision records, including withdrawn ones. |
| `operations/` | Running, configuring, signing and releasing, including the configuration reference, error codes, troubleshooting and the colour check guide. |
| `security/` | Trust boundaries, process models, SELinux, clipboard and supply chain. |
| `testing/` | How to run the colour probe matrix and the real-screen harness. |
| `DOC-STANDARD.md` | The house format for `ARCHITECTURE.md` and `AGENTS.md`. |

### `scripts/` — gates and harnesses

| Path | Is |
| --- | --- |
| `ci/` | CI gates: publication hygiene, workspace boundaries, action pins, component matrix, supply chain and release manifest generation, validation profiles. |
| `review/` | Local review hooks: `doc-guard.sh`, secret scan, redaction, pre-commit hook installer. |
| `lab/` | Lab-only harnesses: the real-screen latency harness (`realscreen/`), Wayland and X11 probes, and the virtual USB host controller lab driver (`usb-vhci/`). Never run against production. |
| `check_shared_contracts.py`, `shared_adoption_index.py` | Shared-crate consumer and impact checks. |
| `error_code_reference.py`, `pier_config_reference.py` | Extract every error code and every `pier.json` key from the code, so the check fails when [Error codes](docs/operations/error-codes.md) or the [configuration reference](docs/operations/config-reference.md) misses one. |
| `validate_observability.py`, `observability_event_definitions.py` | Observability event schema validation. |
| `generate-third-party-notices.py` | Regenerates `legal/THIRD_PARTY_NOTICES.md`. |
| `verify-*`, `test-openh264-assembly.sh`, `verify_quic_product_binary.py` | Vendored-source and product-binary verification. |
| `sync-to-build-host.sh` | Copies the git source allowlist of a working tree to a build host; ignored files stay local. |
| `test_*.py` | Unit tests for the Python gates. |

### `tools/` — internal tools (Shared/Architecture)

| Path | Is |
| --- | --- |
| `session-probe/` | A Python Deck stand-in that completes a full session handshake and reports whether frames arrive. |
| `region-patch-benchmark/` | A research-only Rust harness for region patch framing; a workspace member for CI, never in a product. |

### `support/` — local only

Gitignored. Holds support bundles copied from hosts (`arcen-support-*.zip`),
which contain logs, configuration and host identity. Never committed. The
format is documented in [`docs/operations/support-bundles.md`](docs/operations/support-bundles.md).

### `tests/` — cross-component tests (Shared/Architecture and Release/Security)

| Path | Is |
| --- | --- |
| `e2e/` | End-to-end flows; today the observability fixtures for Linux and Windows sessions. |
| `fixtures/` | Shared runtime fixtures (multi-monitor outcomes, region input). |
| `hardware/` | Hardware acceptance contract; deferred until runner isolation exists. |
| `recovery/` | Interruption and recovery scenarios. |

### `third_party/` — vendored dependencies (Release/Security)

| Path | Is |
| --- | --- |
| `apple-cf-0.9.3-arcen1/` | Patched Core Foundation bindings for macOS 12 support. |
| `videotoolbox-0.18.1-arcen1/` | Patched VideoToolbox bindings for macOS 12 support. |
| `opusic-sys-0.7.3-arcen1/` | Opus bindings, patched to link the static C runtime on MSVC. |
| `nv-codec-headers/` | Only the MIT-licensed NVENC API header, used to generate the encoder bindings. |

Each `-arcen1` crate carries `ARCEN_NOTICE.md`, its patch files and an
`ARCEN_SOURCE_MANIFEST.sha256` that `scripts/verify-vendored-source.sh` checks.

### `legal/` (Release/Security)

| Path | Is |
| --- | --- |
| `ORIGINS.md` | Statement that Arcen is original first-party work. |
| `THIRD_PARTY_NOTICES.md` | Full notices for everything that ships in a release artefact. Exhaustive by policy. |
