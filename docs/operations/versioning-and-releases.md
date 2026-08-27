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

Commit subjects follow Conventional Commits:

- `feat`: product or API capability
- `fix`: defect correction
- `perf`: measured performance improvement
- `refactor`: behavior-preserving restructure
- `test`, `docs`, `build`, `ci`, `chore`: corresponding maintenance changes

Use `!` and a `BREAKING CHANGE:` footer for intentional compatibility breaks.
The commit history informs release notes but does not replace review.

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

The intended protected staging and production contract is documented in
[Release automation](release-automation.md). Those workflows are currently
absent, not merely disabled; Release/Security must rebuild and review them after
the repository environments, variables, platform packaging hooks, and
signing/notarization hooks are defined.

## Toolchain parity

`rust-toolchain.toml` pins an exact compiler version, not `stable`. A release is
three artefacts built on three machines, and a floating channel lets those be
three different compilers — and makes a rebuild from the same tag produce a
different binary months later.

Pinning the file is not sufficient on its own, because each platform ignored it
in a different way:

- **macOS** — Homebrew's `rust` formula installs `cargo` ahead of rustup's shims
  on `PATH` and does not read `rust-toolchain.toml` at all. The Deck, the
  artefact users actually download, was the one built unpinned.
  `packaging/macos/build-deck-app.sh` now resolves the pinned channel, invokes
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
