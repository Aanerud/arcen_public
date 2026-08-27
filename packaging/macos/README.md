# macOS Deck Packaging

`build-deck-app.sh` builds `arcen-deck-macos` and assembles `Arcen Deck.app`
using the checked-in plist and entitlements. It has three explicit,
mutually exclusive modes and never discovers or ranks keychain identities:

- **Unsigned** (default): rejects any protected release or development input.
- **`--dev-sign`**: explicit local development signing. Requires external
  `ARCEN_DEV_PROVISIONING_PROFILE` and `ARCEN_DEV_CODESIGN_IDENTITY` (an Apple
  Development identity/profile). Verifies team/bundle/entitlements the same
  way release does, but never notarizes, staples, or runs Gatekeeper.
- **`--release`**: explicit Developer ID release signing. Requires external
  `ARCEN_PROVISIONING_PROFILE`, `ARCEN_CODESIGN_IDENTITY`, and
  `ARCEN_NOTARY_KEYCHAIN_PROFILE`.

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
