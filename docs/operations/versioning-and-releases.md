# Versioning and Release Metadata

## Versions

Arcen uses Semantic Versioning for product and shared workspace releases. During
initial development, `0.y.z` may contain intentional compatibility changes
documented in release notes. The root `VERSION`, workspace package version, tag,
and release manifest must match exactly.

Tags use `vMAJOR.MINOR.PATCH` with SemVer prerelease identifiers when needed.
Changing a version requires a reviewed changelog entry and compatibility impact
assessment.

## Change metadata

Commit subjects name the area and then say what changed in behaviour, in one
plain sentence: `linux: type committed text on Xorg sessions`,
`protocol: srgb_transfer client capability (additive)`,
`docs: colour check guide, README, troubleshooting and changelog`. Areas are
the component (`shared`, `protocol`, `media`, `linux`, `windows`, `macos-pier`,
`deck`, `capenc`, `packaging`, `docs`, `ci`), optionally narrowed
(`capenc(windows)`). Conventional Commit types in front of the area
(`fix(linux):`, `feat(capenc):`) are also accepted and common in the history.

State an intentional compatibility break in the commit body and in
`CHANGELOG.md`. The commit history informs release notes but does not replace
review; user-visible changes go under `[Unreleased]` in `CHANGELOG.md` as they
land.

## Release manifest

Each distributable release must record:

- Arcen version and Git commit
- Rust compiler and target triple
- lockfile digest and enabled Cargo features
- artifact names, sizes, and cryptographic checksums
- build workflow identity and provenance reference
- generated software bill of materials
- reviewed third-party notices
- signing or notarization status for each platform
- known compatibility and migration notes

Native-codec releases must also record the supplied upstream source commit,
target C++ compiler and NASM versions, static-runtime posture, enabled
source-only feature, and package dependency/payload inspection. For portable
H.264 this includes the Rust 1.89 effective floor imposed by `wide` 1.1.1, even
though the dependency-light default workspace remains at 1.85.

No artifact is a production release until Release/Security approves the
manifest, provenance, notices, and platform-specific signing result.
Software H.264 additionally requires legal approval of the actual H.264
distribution/patent posture and recorded physical soak/performance/allocation
acceptance; BSD-2-Clause is not treated as patent clearance.

The intended protected staging and production contract is stated under
"Release artifact boundary" in
[the supply-chain policy](../security/supply-chain.md#release-artifact-boundary).
Those workflows are currently absent, not merely disabled; Release/Security must rebuild and review them after
the repository environments, variables, platform packaging hooks, and
signing/notarization hooks are defined.

## Telling an official build apart

Anyone may build Arcen, and every build says which build it is: the hello a
Deck and a Pier exchange carries a build identity (`build_id`,
`source_revision`, `artifact_sha256`, `signing_state`), and both products log
it. An unstamped build reports `build_id` `development`.

Official builds from TUTTIFRUTTI AS are made from a clean checkout of the
published release tag and stamped by `scripts/release/official-build-id.sh`:

```sh
git checkout v0.19.0
eval "$(scripts/release/official-build-id.sh)"
# ARCEN_BUILD_ID=official-20261109T120000Z-v0.19.0-0123456789ab
# ARCEN_SOURCE_REVISION=<the tagged commit>
```

so the build ID names the release tag and the public commit it was built
from, and `arcen_protocol::build_identity::has_official_build_id_shape`
recognises it. The shape is a label, not proof — anyone can stamp it. To
verify that a binary is official, check that its code signature belongs to
TUTTIFRUTTI AS and that its `artifact_sha256` is listed in the `SHA256SUMS` of
the official release, whose notes link the exact source tag.

## Toolchain parity

`rust-toolchain.toml` pins an exact compiler version, not `stable`. A release is
three artefacts built on three machines, and a floating channel lets those be
three different compilers — and makes a rebuild from the same tag produce a
different binary months later.

Pinning the file is not sufficient on its own, because each platform can
bypass it in a different way, so each build script enforces it:

- **macOS** — Homebrew's `rust` formula installs `cargo` ahead of rustup's shims
  on `PATH` and does not read `rust-toolchain.toml` at all.
  `packaging/macos/build-deck-app.sh` therefore resolves the pinned channel, invokes
  cargo through `rustup run`, and aborts if the resolved `rustc` is not the
  pinned version.
- **Windows** — rustup resolves the host triple separately from the channel, and
  a machine whose default host is `x86_64-pc-windows-gnu` will build the whole
  workspace with the mingw ABI. `arcen-credential-provider` is a COM DLL loaded
  by LogonUI and must be MSVC. `hosts\windows\build.cmd` reads the version from
  `rust-toolchain.toml`, appends `-x86_64-pc-windows-msvc`, and refuses to
  continue if the resolved host tuple is not MSVC.
- **Linux** — resolves through rustup's shims, so the pin applies unaided.

Before cutting a release, record `rustc -Vv` from all three build machines in
the release notes and confirm they match. Three artefacts of one version built
by three compilers is not a release; it is three releases.

Bump the pin deliberately, and re-record all three when you do.
