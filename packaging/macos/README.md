# macOS Deck Packaging

`build-deck-app.sh` builds `arcen-deck-macos` for one architecture and
assembles `dist/macos/Arcen Deck.app` (Apple silicon) or
`dist/macos/Arcen Deck Intel.app` (Intel) using the checked-in plist and
entitlements (see [Architecture and minimum macOS](#architecture-and-minimum-macos)).
Besides the default unsigned assembly it has three explicit, mutually
exclusive signing modes and never discovers or ranks keychain identities:

- **Unsigned** (default): rejects any protected release or development input.
- **`--dev-sign`**: explicit local development signing. Requires external
  `ARCEN_DEV_PROVISIONING_PROFILE` and `ARCEN_DEV_CODESIGN_IDENTITY` (an Apple
  Development identity/profile). Verifies team/bundle/entitlements the same
  way release does, but never notarizes, staples, or runs Gatekeeper.
- **`--developer-id-sign`**: explicit local Developer ID signing with the
  release profile and identity, verified like `--release` but not notarized
  and not zipped. For same-machine testing of Developer ID-only permissions.
- **`--release`**: explicit Developer ID release signing. Requires external
  `ARCEN_PROVISIONING_PROFILE`, `ARCEN_CODESIGN_IDENTITY`, and
  `ARCEN_NOTARY_KEYCHAIN_PROFILE`.

The one app contains both presentation pipelines: ordinary SDR video for
Auto/Speed and a dedicated native ten-bit Metal layer for Grading/HDR. PQ/EDR
is enabled only when the resolved host transfer remains PQ; no separate HDR
bundle or entitlement is installed.

The script does not discover, inventory, log, or synthesize profiles, identities, or
credentials, and never exposes protected profile contents. Before decoding or
embedding a private snapshot of the profile, a checked-in verifier uses
supported Security.framework CMS signer-status and `SecTrust` APIs, then
requires the Apple macOS provisioning-profile signer identity and exact reviewed
WWDR intermediate and Apple root certificate DER digests — the same check for
both signing modes, since Apple signs Development and Distribution profiles
with the same provisioning-profile signer. Any signature or
trust failure aborts packaging; `security cms` decode status is never
treated as a trust decision.

Both signed modes locally decode only nonsensitive profile metadata and reject
an expired or mismatched team/app profile, or any profile that is not an
entitlement superset of `Deck.entitlements`. After hardened-runtime signing,
each mode verifies the resulting signature against its own certificate class —
`--dev-sign` requires an "Apple Development" identity, `--release` requires a
"Developer ID Application" identity — so a development signature can never be
mistaken for release evidence or vice versa. Only `--release` requires and
performs notarization, stapling, staple validation, and a Gatekeeper
assessment. Both modes require Python 3. Ordinary unsigned assembly rejects
every protected input, release or development.

Deck is not sandboxed (no `com.apple.security.app-sandbox` entitlement).
`Deck.entitlements` therefore requests only the identity keys needed to embed
a provisioning profile plus two profile-authorized restricted capabilities,
`com.apple.developer.sustained-execution` and
`com.apple.developer.associated-domains`. App-Sandbox-only device/network keys
(`com.apple.security.network.client`, `com.apple.security.device.audio-input`)
are no-ops without App Sandbox and are not requested; microphone access is
governed entirely by TCC (`NSMicrophoneUsageDescription` plus the explicit
runtime `AVAudioEngine` consent check in `microphone.rs`), not by an
entitlement. `com.apple.security.device.input-monitoring` is also not
requested: it is an undocumented key, not a code-signing entitlement Apple
publishes, and Input Monitoring is a TCC permission that default in-window
AppKit tablet capture never needs — it is relevant only to the quarantined,
non-default experimental raw-HID path. See
`clients/macos/CERTIFICATES.md` and `clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`
for the full rationale and the real provisioning profile's contents.

On macOS the native tests always compile the verifier and distinguish malformed
CMS input from trust rejection. The optional adversarial test accepts an
externally managed, nonsensitive, cryptographically valid untrusted CMS path
through `ARCEN_TEST_UNTRUSTED_PROVISIONING_PROFILE` and requires the distinct
untrusted-signer result. Tests do not generate, inspect, or log profiles,
certificates, or keys.

The Deck is a decoder and does not enable OpenH264. Bundle checks reject dynamic
or nested Opus/OpenH264 dylibs, frameworks, static archives, or import
libraries. No codec payload may be added to preserve the PR #43/#44 packaging
shape. `NSMicrophoneUsageDescription` and explicit runtime consent cover the
default-off, explicitly negotiated microphone feature. Hosted CI can assemble
and inspect the bundle but does not claim a
Developer ID signature, notarization, or staple; release provenance, upgrades,
and uninstall behavior remain Release/Security work.


## Architecture and minimum macOS

Apple silicon and Intel Macs get two separate apps, built from the same source
by the same script. There is no universal binary.

| `--arch` | App | Release zip |
| --- | --- | --- |
| `arm64` | `Arcen Deck.app` | `Arcen-Deck-<version>-macOS.zip` |
| `x86_64` | `Arcen Deck Intel.app` | `Arcen-Deck-Intel-<version>-macOS.zip` |

The architecture comes from `--arch arm64|x86_64` (or `--arch=…`), else
`ARCEN_DECK_ARCH`, else the building Mac (`uname -m`). The script always passes
the matching `--target` (`aarch64-apple-darwin` or `x86_64-apple-darwin`) to
cargo for both the Deck and the USB helper, so binaries land in
`target/<triple>/release/` and either app can be built on either kind of Mac.
The release machine is Apple silicon: it cross-compiles the Intel Deck and
signs, notarizes and staples both apps in one place:

```sh
rustup target add --toolchain 1.96.1 x86_64-apple-darwin   # once
packaging/macos/build-deck-app.sh --arch arm64 --release
packaging/macos/build-deck-app.sh --arch x86_64 --release
```

A missing Rust target stops the build with the `rustup target add` command to
run. `--no-build` reads the binaries from `target/<triple>/release/` too.

The Intel app differs from the Apple-silicon app only in its binaries and in
`CFBundleName`/`CFBundleDisplayName` ("Arcen Deck Intel"), which the script
writes into the bundle's copy of `Deck-Info.plist` and verifies. Both keep
`CFBundleIdentifier` `deck.arcen.tech`, because that is the only App ID with a
Developer ID provisioning profile carrying the Deck's entitlements (Hard USB
and virtual HID included); the profile must list every certificate that signs
with it. The consequence: macOS treats the two apps as one application. Install
and run one of them per Mac. They share settings and saved connections
(`~/Library/Application Support/Arcen`), keychain items, and privacy (TCC)
grants such as Microphone, and with both installed Launch Services may open
either copy. An Intel Deck run under Rosetta on Apple
silicon works but logs a warning to install `Arcen Deck` instead.

`LSMinimumSystemVersion` in `Deck-Info.plist` (12.0) is the single source for
the minimum macOS. The script exports it as `MACOSX_DEPLOYMENT_TARGET`, and it
refuses a conflicting value in the environment. After building, it requires
that the Deck and the embedded USB helper each carry exactly one architecture,
the one requested, and an `LC_BUILD_VERSION` `minos` equal to the plist
value. Without this, Rust picks its own default and the app either refuses a
supported macOS or dies in dyld on it.

Two crates are path-patched in `third_party/` so the Deck loads on macOS 12 and
builds with the Swift 5.7 toolchain of the last Xcode macOS 12 supports.
`apple-cf` compiles its Swift bridge with `swiftc` for the target architecture
Cargo is building for and the deployment target (for example
`x86_64-apple-macosx12.0` when cross-compiling the Intel Deck), and
`videotoolbox` decodes through `VTDecompressionSessionDecodeFrame` rather than a
newer entry point. Each directory's `ARCEN_NOTICE.md` lists the changes, and
`scripts/verify-vendored-source.sh` checks each tree against its manifest
before every build.

Native tablet (Hard USB) installs its helper with `SMAppService`, which needs
macOS 13. On macOS 12 the Deck never calls it; the Input settings show
"Requires macOS 13" and Tablet support remains available.

What differs between the two apps at run time is decided in one place, the
Deck platform profile (`arcen_media::deck_platform`, see
[`clients/macos/ARCHITECTURE.md`](../../clients/macos/ARCHITECTURE.md#deck-platform-profile)).
On an Intel Mac, VideoToolbox may decode in software when the GPU has no
decoder for the stream; that is expected there. The Deck's capability probe
advertises only what a real decode session accepted, so the Pier negotiates
within it, and the Intel Deck never advertises AV1. `arcen-deck
platform-report` prints the profile and the probe without opening a window.

Building, signing and notarizing the Pier package, and the install-time
traps that go with it, are documented in
[`hosts/macos/PACKAGING.md`](../../hosts/macos/PACKAGING.md).

## macOS Pier packaging status

This directory now contains development packaging for the macOS Pier:
`build-pier-app.sh` assembles `dist/macos/Arcen Pier.app` and
`dist/macos/Arcen Agent Helper.app`, and `build-pier-pkg.sh` builds
`dist/macos/ArcenPier-<version>.pkg` around them. It is not a published host
release and must not be described as one.

One packaging bug is worth keeping visible. `build-pier-app.sh` used to check
that `target/release/arcen-pier-macos` existed and then package whatever file
was there. That shipped bundles older than their source: a fix could be
written, tested, packaged, signed, installed, and then measured still failing
because the binary inside the package predated the fix. The script now runs
`cargo build --locked --release -p arcen-pier-macos` before assembly.

macOS still does not have the Linux/Windows host install contract. The package
is a development artefact, not a Release/Security-reviewed distribution with
completed cold-boot LoginWindow qualification, upgrade/rollback acceptance,
permission onboarding, and uninstall cleanup. Do not describe the Deck package
as a Pier installer, and do not use this development package as evidence that
pre-login operation is shipped. It must not run the network or media stack as
root, and it must preserve the FileVault pre-boot boundary documented in
`docs/todo/macos-pier.md`.
