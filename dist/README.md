# Release artefacts

This directory is where release binaries are assembled by hand before being
attached to a GitHub release. Only this file is tracked; the binaries never are.

Build them with:

| Artefact | How |
| --- | --- |
| `install-arcen-pier-<version>-linux-x86_64` | On a Linux host: `cargo build --locked --release -p arcen-pier-linux`, then `ARCEN_PIER_BINARY=target/release/arcen-pier cargo build --locked --release -p arcen-pier-linux-installer` |
| `install-arcen-pier-<version>-windows-x64.exe` | On a Windows host: `hosts\windows\build.cmd`, which enters the MSVC environment itself and produces `target\arcen-windows-x64\install-arcen-pier.exe` |
| `Arcen-Deck-<version>-macOS.zip` | On macOS: `packaging/macos/build-deck-app.sh --release`, which signs, notarises and staples |

All three must be built from the same commit, with the toolchain pinned in
`rust-toolchain.toml`. Record `rustc -Vv` from each machine in the release
notes: three artefacts of one version built by three different compilers is not
a release, it is three releases.

## Checksums

Regenerate after every rebuild:

```sh
cd dist && shasum -a 256 install-arcen-pier-* Arcen-Deck-*.zip > SHA256SUMS.txt
```

Publish `SHA256SUMS.txt` alongside the binaries so a downloader can verify what
they got.

## Signing

The Linux and Windows installers are **not signed**. Windows SmartScreen will
warn; that is expected for an unsigned binary from a project with no
code-signing certificate, and the checksum is how you verify it instead.

The macOS Deck **must** be signed with a Developer ID identity, notarised and
stapled before it is given to anyone. An unsigned or un-notarised bundle opens
as *"Arcen Deck.app is damaged and can't be opened"* on any machine but the one
that built it, which looks like corruption rather than a missing signature.
`packaging/macos/build-deck-app.sh --release` requires
`ARCEN_PROVISIONING_PROFILE`, `ARCEN_CODESIGN_IDENTITY` and
`ARCEN_NOTARY_KEYCHAIN_PROFILE`; see
[`docs/operations/macos-signing.md`](../docs/operations/macos-signing.md).

## Before attaching anything to a release

- `scripts/ci/check-publication-hygiene.sh` passes.
- Every artefact was rebuilt from the tagged commit — not carried over from a
  previous build. A binary that predates a fix silently ships the bug.
- The Deck bundle reports the right version:
  `plutil -extract CFBundleShortVersionString raw "Arcen Deck.app/Contents/Info.plist"`.
- Each binary's `--version` prints the AGPL notice and the source URL.
