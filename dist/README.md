# Release artefacts

This directory is where release binaries are assembled by hand before being
attached to a GitHub release. Only this file is tracked; the binaries never are.

## Layout

| Path | Holds |
| --- | --- |
| `dist/macos/` | What the macOS build scripts write: `Arcen Deck.app`, `Arcen Pier.app`, `Arcen Agent Helper.app`, `ArcenPier-<version>.pkg`, and with `--release` `Arcen-Deck-<version>-macOS.zip` |
| `dist/linux/` | The Linux installer, copied back from the Linux build host |
| `dist/windows/` | The Windows installer, copied back from the Windows build host |
| `dist/releases/<version>/` | Exactly the files attached to that GitHub release, flat, plus `SHA256SUMS.txt` and the `rustc -Vv` of each build machine |

The platform directories are working output and are overwritten by the next
build. A release directory is written once, from one source identity, and is
never mixed with another version. Nothing else belongs here: signing profiles
and keys live outside the repository (see
[`docs/operations/macos-signing.md`](../docs/operations/macos-signing.md)).

There is one Pier installer per host OS (a single-file installer for Linux and
Windows, an installer package for macOS) and one Deck archive. Auto, Speed,
Grading, and HDR support are built into those same artifacts; there is no HDR
add-on or separate fidelity package. The Linux Pier binary contains both the
NvFBC eight-bit and XShm ten-bit paths. The Windows Pier binary contains the
eight-bit DDA/WGC, FP16 Grading, and verified FP16 HDR paths. The Deck archive
contains both ordinary SDR presentation and the dedicated ten-bit Metal layer.

Build them with:

| Artefact | How |
| --- | --- |
| `install-arcen-pier-<version>-linux-x86_64` | On a Linux host: `cargo build --locked --release -p arcen-pier-linux`, then `ARCEN_PIER_BINARY=target/release/arcen-pier cargo build --locked --release -p arcen-pier-linux-installer`; copy `target/release/install-arcen-pier` to `dist/linux/` |
| `install-arcen-pier-<version>-windows-x64.exe` | On a Windows host: `hosts\windows\build.cmd`, which enters the MSVC environment itself and produces `target\arcen-windows-x64\install-arcen-pier.exe`; copy it to `dist/windows/` |
| `ArcenPier-<version>.pkg` | On macOS: `packaging/macos/build-pier-pkg.sh --identity "Developer ID Application: …" --installer-identity "Developer ID Installer: …" --with-virtual-hid --provisioning-profile <file> --notary-profile <profile>`, which signs the app and package, notarises, staples and writes `dist/macos/ArcenPier-<version>.pkg`. The macOS Pier is not yet a supported host release (see the top-level README). |
| `Arcen-Deck-<version>-macOS.zip` | On macOS: `packaging/macos/build-deck-app.sh --release`, which signs, notarises, staples and writes `dist/macos/Arcen-Deck-<version>-macOS.zip` |

All of them must be built from the same source identity and build ID, with the
toolchain pinned in `rust-toolchain.toml`. A deliberately uncommitted candidate
must say `-dirty`; it must not pretend to be the clean HEAD commit. Record
`rustc -Vv` from each machine in the release notes: three artefacts of one
version built by three different compilers is not a release, it is three
releases.

## Checksums

Regenerate after every rebuild, inside the release directory:

```sh
cd dist/releases/<version> && shasum -a 256 install-arcen-pier-* ArcenPier-*.pkg Arcen-Deck-*.zip > SHA256SUMS.txt
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

The macOS Pier package follows the same rule, and more strictly: its app
carries the virtual-HID entitlement, which only takes effect with the matching
provisioning profile embedded, and Gatekeeper refuses an unnotarised package
outright. Check it before attaching it:
`pkgutil --check-signature dist/releases/<version>/ArcenPier-<version>.pkg` and
`spctl --assess --type install -v dist/releases/<version>/ArcenPier-<version>.pkg`.

## Before attaching anything to a release

- `scripts/ci/check-publication-hygiene.sh` passes.
- Every artefact was rebuilt from the tagged commit — not carried over from a
  previous build. A binary that predates a fix silently ships the bug.
- The embedded build identity matches across all artifacts and truthfully
  says whether the source was dirty.
- The Deck bundle reports the right version:
  `plutil -extract CFBundleShortVersionString raw "dist/macos/Arcen Deck.app/Contents/Info.plist"`.
- Each binary's `--version` prints the AGPL notice and the source URL.
