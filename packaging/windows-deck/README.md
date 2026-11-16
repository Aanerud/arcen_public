# Windows Deck packaging

Two MSIs for Windows 11 x64, built from one source identity by
[`build.ps1`](build.ps1):

| Artefact | Installs | Privilege |
| --- | --- | --- |
| `Arcen Deck.msi` | `C:\Program Files\Arcen\Deck\arcen-deck.exe`, the five legal files, an all-users Start-menu shortcut "Arcen Deck", and WER LocalDumps for `arcen-deck.exe` | Per-machine install (one UAC prompt). The Deck itself never runs elevated; the package does not launch it. |
| `Arcen USB Component.msi` (optional) | `C:\Program Files\Arcen\USB\{arcen-usb-service.exe, arcen-usb-prepare.exe}`, the five legal files, and the LocalSystem service `ArcenUsbComponent` | The only privileged part of the Windows Deck, installed separately so an ordinary Deck install carries no privileged process (ADR 0024). |

The five legal files are `LICENSE`, `LICENSE-EXCEPTION.md`, `NOTICE`,
`TRADEMARKS.md` and `THIRD_PARTY_NOTICES.md`, beside the executables in each
package, as in every Arcen artefact.

## Build

On a Windows x64 machine with Visual Studio's x64 C++ tools, the Windows SDK,
CMake, rustup and Python 3:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\windows-deck\build.ps1 `
    -OutputDirectory C:\arcen-build\wd-package\out `
    -CargoTargetDirectory C:\arcen-build\wd-package\target `
    -ToolsDirectory C:\arcen-build\wd-package\tools
```

`-BuildId` and `-SourceRevision` (or `ARCEN_BUILD_ID` / `ARCEN_SOURCE_REVISION`)
default to `<UTC stamp>-<version>-<commit>` and the commit when the tree is a
git checkout; a copied tree must pass both. Official builds use
`scripts/release/official-build-id.sh`. The script enters the MSVC
environment itself (`vswhere` + `vcvars64.bat`), so any shell works.

It then, in order:

1. checks that `legal/THIRD_PARTY_NOTICES.md` matches `Cargo.lock`
   (`scripts/generate-third-party-notices.py --check`) and verifies the
   vendored Opus source;
2. builds `arcen-deck.exe`, `arcen-usb-service.exe` and `arcen-usb-prepare.exe`
   in release with the pinned toolchain (`rust-toolchain.toml`, MSVC host),
   `+crt-static` and `/MT` like the Windows Pier, so no Visual C++
   Redistributable is needed, and with the build identity in
   `ARCEN_BUILD_ID`, `ARCEN_SOURCE_REVISION`, `ARCEN_FEATURE_PROFILE` and
   `ARCEN_SIGNING_STATE`;
3. links a **Win32 version resource** into each executable: company, product,
   `FileVersion` = the release version, `ProductVersion` =
   `<version> (build <build ID>)`, and `ArcenBuildId` / `ArcenSourceRevision`
   strings. The Deck also gets its icon (from `icon/`, the macOS Deck's
   artwork), and `arcen-usb-prepare.exe` a `requireAdministrator` manifest.
   The resource is compiled with `rc.exe` and handed to the final link only
   (`cargo rustc … -- -C link-arg=<res>`), so no crate manifest or build
   script changes and no dependency sees it. Explorer's Properties › Details
   shows the build ID;
4. refuses the build unless each version resource carries the build ID,
   `arcen-deck.exe --version` prints `build_id=<build ID>`, no executable
   imports a dynamic C runtime, neither USB executable is a placeholder, and the
   prepare tool's manifest requires elevation;
5. signs the executables when a certificate is configured (below);
6. builds both MSIs with WiX, runs ICE validation and
   [`verify-msi.ps1`](verify-msi.ps1), signs the MSIs when configured;
7. writes `SHA256SUMS.txt` (LF, `sha256sum -c` format) and
   `build-manifest.json` (below) beside them.

## Release candidate or lab preview

Every build states which it is, in the MSIs (`ARCENRELEASECANDIDATE`, and an
Apps & features comment beginning "Lab preview, not a release candidate.")
and in `build-manifest.json` (`release_candidate`, `not_a_candidate_because`).
A **release candidate** needs all of:

- one exact revision: a full 40-hex `SourceRevision`, and a clean tree
  (detected from git; a copied tree is `unknown` unless the copier passes
  `-SourceTreeState clean`, which a checkout contradicting it refuses);
- the real applications: `build.ps1` refuses the Deck's version-only stub and
  the USB placeholders.

`-LabPreview` packages a stub Deck for work on the installers themselves and
marks everything a lab preview. Evidence for Beta 1 (install, upgrade,
repair, uninstall, cleanup) counts only for a release candidate, and only for
the MSI hashes in that build's `SHA256SUMS.txt`. `build-manifest.json` binds
them to their inputs: revision and tree state, `rustc -vV`, the WiX hash, the
SHA-256 of `Cargo.lock`, of the five legal files and of the packaging sources,
and each executable's and MSI's hash, product version and Authenticode state.

## Why WiX 5.0.2, and no WiX extension

| Option | Licence and terms | Verdict |
| --- | --- | --- |
| **WiX Toolset 5.0.2** | Source and binaries under MS-RL; a build tool, not shipped | **Chosen.** Pinned by version and by the SHA-256 published in the release's `wix-cli-x64.metadata.json`; unpacked with `msiexec /a` (an administrative image), so nothing is installed or registered on the build machine and no .NET SDK is needed. |
| WiX 6 and 7 | Source MS-RL, but the released binaries carry the Open Source Maintenance Fee EULA (`OSMFEULA.txt`, from v6.0.0, April 2025): revenue-generating users above the threshold pay a fee | Not used. `build.ps1` refuses any `wix.exe` that is not 5.0.2. Building WiX 6 from source would avoid the EULA but makes us maintain a .NET toolchain. |
| Rust `msi` crate (MIT) | Clean | Writes MSI databases only; tables, sequences, the cabinet, ICE validation and service tables would all be ours to get right. Too much new surface for Beta 1. |
| `wixl` (msitools) | LGPL-2.1-or-later build tool | Runs off Windows, but implements only a subset of the WiX schema, and an LGPL tool in the release path needs a Release/Security decision. |

`deny.toml` governs Rust crates; WiX is not one, and none of it ships. That
holds only if no WiX code ends up **inside** an MSI. WiX extensions do exactly
that: `WixToolset.UI.wixext` adds dialogs and bitmaps, `WixToolset.Util.wixext`
adds a custom-action DLL. Both packages therefore use only native Windows
Installer tables, and `verify-msi.ps1` fails a package with any Binary-table
row or any DLL, script or embedded-EXE custom action. The cost is basic UI: a
progress bar and UAC, with no licence or folder dialog.

## Package behaviour

**Install location and UI.** `Program Files\Arcen\Deck` and `…\USB`, beside
an existing `…\Pier`. Windows 11 x64 only: the launch condition reads the real
build number (the installer engine reports 603 for every Windows 10/11).

**Upgrade.** `MajorUpgrade` with `Schedule="afterInstallValidate"`: the
previous build is removed completely before the new one is installed, so two
builds of one release (equal file versions) can never leave an old file
behind. Builds of the same version upgrade each other; installing an older
version over a newer one is refused. ProductCode changes every build; the
UpgradeCodes are fixed:

| Package | UpgradeCode |
| --- | --- |
| Arcen Deck | `2DDC0F1A-DB47-42C2-AE74-EEFF30428FC4` |
| Arcen USB component | `652B4B54-163A-4D12-8DCC-53B868E76B4F` |

Never change them: a new UpgradeCode makes the next release install beside
the old one instead of replacing it.

**Deck uninstall** removes the files, the shortcut and the WER key, and the
`Arcen` folder only when nothing else (a Pier) is in it. Per-user data stays,
as on the Mac: settings, saved connections and trusted pins in
`%APPDATA%\Arcen\Deck`, logs and crash dumps in `%LOCALAPPDATA%\Arcen\Deck`.

**WER LocalDumps.** `HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\arcen-deck.exe`:
`DumpType` 1 (minidump only; a full dump could hold a password between
typing and zeroizing), `DumpCount` 5, `DumpFolder`
`%LOCALAPPDATA%\Arcen\Deck\CrashDumps` (`REG_EXPAND_SZ`, expanded for the
user whose Deck crashed). The Deck writes the same values per user (HKCU)
itself, so a Deck run without the MSI keeps dumps too.

**USB component.** The service `ArcenUsbComponent` runs as LocalSystem, own
process, **automatic** start: the Deck reports a stopped component and greys
Native tablet out, and the service's start-up recovery gives a tablet its own
driver back after a crash. Failure actions: restart after 5 s, again after
30 s, then stay stopped (reset after a day), set by the in-box `sc.exe` as
SYSTEM before the service first starts (not on repair), because Windows Installer's
`MsiServiceConfigFailureActions` fails with error 1939 and WiX's
`util:ServiceConfig` would embed a WiX DLL. Nothing is put in the driver store
and no certificate is trusted at install time; the service creates
`%ProgramData%\Arcen\USB` with its own protected ACL and stages a prepared
package only for the length of a session.

Both install folders have private property Ids, so `msiexec` cannot be told
to install the LocalSystem service (or the Deck) somewhere a user could write.

**USB component uninstall** stops the service, then runs
`arcen-usb-service.exe uninstall` as SYSTEM (deferred, not impersonated)
before the service is deleted and the files removed. That command restores
any claimed tablet from the journal, removes every `arcen_usb_*` driver-store
package, removes every "Arcen USB component" certificate from
`LocalMachine\Root` and `TrustedPublisher`, deletes `%ProgramData%\Arcen\USB`,
checks again and exits non-zero on any leftover. A non-zero exit rolls the
uninstall back, so trust can never be left behind silently; the `LEFTOVER`
lines are then in `%ProgramData%\Arcen\usb-component-uninstall.txt` (or
`%SystemRoot%\Temp\arcen-usb-component-uninstall.txt`), and running
`arcen-usb-service.exe uninstall` from an elevated prompt shows them too. The
step runs whenever the service executable is removed, except by a major
upgrade, so prepared tablets survive an update. Windows Installer then
removes `%ProgramData%\Arcen` if it is empty, which it is unless a Pier is
installed.

## Signing

Arcen ships **unsigned**. The project has no code-signing certificate and
does not plan to obtain one, so nothing here depends on one. `build.ps1`
prints

```text
==> Authenticode: SKIPPED - ARCEN_SIGN_CERT_THUMBPRINT is not set. The executables and MSIs are UNSIGNED; …
```

and stamps `unsigned` into the build, which `arcen-deck.exe --version` and
`build-manifest.json` report. The script still has an unused hook
(`ARCEN_SIGN_CERT_THUMBPRINT` + `ARCEN_SIGN_TIMESTAMP_URL`, `signtool` with
SHA-256 and an RFC 3161 timestamp) so that a fork with its own certificate
does not need to change it; no signing material is ever stored in the
repository or the build directory.

### What users see with an unsigned MSI

- **SmartScreen.** An MSI downloaded with a browser carries the Mark of the
  Web. Opening it shows "Windows protected your PC — Microsoft Defender
  SmartScreen prevented an unrecognized app from starting", with **Don't run**
  as the only button (seen on Windows 11 25H2 build 26200 with the Beta 1
  candidate, 2026-11-15; nothing is installed). The user chooses **More info**,
  then **Run anyway**. Copying the file without the mark (USB stick, network
  share, `scp`) or ticking **Unblock** in its Properties avoids the dialog.
- **UAC.** The elevation prompt for a per-machine install says
  "Publisher: Unknown" and shows the MSI's random temporary name instead of
  "Arcen Deck".
- **Smart App Control.** On a PC with Smart App Control in enforcement mode,
  an unsigned installer from the internet that Microsoft's cloud service has
  no verdict for is blocked, with no "Run anyway"
  ([Smart App Control](https://learn.microsoft.com/en-us/windows/apps/develop/smart-app-control/overview)).
  Such a PC can install Arcen only with Smart App Control turned off; say so
  plainly to testers.
- **The USB component** additionally installs a LocalSystem service. Its trust
  is transient: no driver or certificate at install, a per-install
  self-signed catalog per prepared tablet model (key destroyed after signing),
  everything removed on uninstall, which rolls back rather than leave trust
  behind.

An MSI copied over the network or with `scp` has no Mark of the Web and
installs with `msiexec /i "Arcen Deck.msi"` without the SmartScreen dialog.

## Files

| Path | Is |
| --- | --- |
| `build.ps1` | The build: executables, resources, checks, signing, MSIs, checksums |
| `verify-msi.ps1` | Reads a built MSI and enforces the package contract above |
| `wix/ArcenDeck.wxs` | The Deck package |
| `wix/ArcenUsbComponent.wxs` | The optional USB component package |
| `icon/` | Icon images (from `packaging/macos/AppIcon.svg`; regenerate with `icon/build-icon.sh`) |
| `tests/` | Portable checks of the WiX sources (`python3 -m unittest discover packaging/windows-deck/tests`) |
