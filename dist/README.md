# Release artefacts

This directory is where release binaries are assembled by hand before being
attached to a GitHub release. Only this file is tracked; the binaries never are.

There is one Pier installer per host OS and one Deck archive. Auto, Speed,
Grading, and HDR support are built into those same artifacts; there is no HDR
add-on or separate fidelity package. The Linux Pier binary contains both the
NvFBC eight-bit and XShm ten-bit paths. The Windows Pier binary contains the
eight-bit DDA/WGC, FP16 Grading, and verified FP16 HDR paths. The Deck archive
contains both ordinary SDR presentation and the dedicated ten-bit Metal layer.

Build them with:

| Artefact | How |
| --- | --- |
| `install-arcen-pier-<version>-linux-x86_64` | On a Linux host: `cargo build --locked --release -p arcen-pier-linux`, then `ARCEN_PIER_BINARY=target/release/arcen-pier cargo build --locked --release -p arcen-pier-linux-installer` |
| `install-arcen-pier-<version>-windows-x64.exe` | On a Windows host: `hosts\windows\build.cmd`, which enters the MSVC environment itself and produces `target\arcen-windows-x64\install-arcen-pier.exe` |
| `Arcen-Deck-<version>-macOS.zip` | On macOS: `packaging/macos/build-deck-app.sh --release`, which signs, notarises and staples |

All three must be built from the same source identity and build ID, with the
toolchain pinned in `rust-toolchain.toml`. A deliberately uncommitted candidate
must say `-dirty`; it must not pretend to be the clean HEAD commit. Record
`rustc -Vv` from each machine in the release notes: three artefacts of one
version built by three different compilers is not a release, it is three
releases.

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
- The embedded build identity matches across all three artifacts and truthfully
  says whether the source was dirty.
- The Deck bundle reports the right version:
  `plutil -extract CFBundleShortVersionString raw "Arcen Deck.app/Contents/Info.plist"`.
- Each binary's `--version` prints the AGPL notice and the source URL.
