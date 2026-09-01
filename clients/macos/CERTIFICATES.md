# Apple Developer Certificates — Arcen Deck (macOS)

## Team details

| Field | Value |
|---|---|
| Apple ID | <APPLE_ID_EMAIL> |
| Team name | <Your Organization> |
| Team ID | <APPLE_TEAM_ID> |
| App ID | deck.arcen.tech |
| Domain | arcen.tech (DNS controlled by us) |

---

## Certificates live outside the repo

Signing certificates and their private keys live **only** on the dev machine's
login keychain — never in this repository and never as `.cer`/`.p12` files
checked into git. `packaging/macos/build-deck-app.sh` does not read a
certificate store or directory of its own; `codesign` resolves whatever
identity string it is given from the login keychain at sign time, and the
script never enumerates or auto-selects a keychain identity.

Apple's WWDR/root trust anchors are not shipped as files either. The reviewed
provisioning-profile signer chain (exact SHA-256 DER digests of the "Mac OS X
Provisioning Profile Signing" intermediate and Apple root) is compiled
directly into `packaging/macos/verify-provisioning-cms.c`, which release and
development signing both build and run before ever decoding or embedding an
external profile.

| Certificate class | Typical subject | Used by |
|---|---|---|
| Developer ID Application | <Your Organization> (<APPLE_TEAM_ID>) | `--release` (notarized distribution) |
| Apple Development | an individual Team <APPLE_TEAM_ID> member | `--dev-sign` (local, unnotarized) |

### Installing on a new dev machine

1. Generate a CSR and request the needed certificate(s) from
   developer.apple.com (or Xcode); this creates the private key in the login
   keychain and installs the matching public certificate.
2. Confirm the identity is usable: `security find-identity -v -p codesigning`
   should list it as **valid**. This command is for a human to *inspect* the
   keychain; the build script itself never calls it or otherwise enumerates
   identities.
3. The private key is never exported or shared. A new machine needs its own
   CSR → new certificate from Apple.

---

## How signing works in the build script

`packaging/macos/build-deck-app.sh` never discovers, ranks, or falls back
across keychain identities. It has exactly three mutually exclusive modes,
selected explicitly:

| Mode | Flag | Identity/profile source | Notarized? |
|---|---|---|---|
| Unsigned | *(none)* | none — rejects any protected input | no |
| Development | `--dev-sign` | `ARCEN_DEV_CODESIGN_IDENTITY` / `ARCEN_DEV_PROVISIONING_PROFILE` (Apple Development) | no |
| Release | `--release` | `ARCEN_CODESIGN_IDENTITY` / `ARCEN_PROVISIONING_PROFILE` (Developer ID) / `ARCEN_NOTARY_KEYCHAIN_PROFILE` | yes |

Both signed modes: verify the external provisioning profile's CMS signature
and Apple trust chain before decoding it, validate its team/bundle/entitlement
superset *and* profile class against `Deck.entitlements`, embed it, sign, then
re-validate the resulting signature against the expected certificate class
(`developer-id` or `apple-development`) — see
`packaging/macos/validate_release_inputs.py`. The profile-class check
(`--profile-class release|development`) is defense-in-depth independent of
the certificate-class check: `--release` rejects a profile whose
`get-task-allow` entitlement is `true` (the standard Apple signal for a
debuggable Development profile), and `--dev-sign` requires
`get-task-allow: true`, so a Developer ID distribution profile cannot
silently be reused in development mode. Only `--release` notarizes,
staples, and runs a Gatekeeper assessment.

Signing command (hardened runtime + entitlements, both modes):
```bash
codesign --sign "$SIGN_ID" \
         --entitlements packaging/macos/Deck.entitlements \
         --options runtime \
         --force --deep \
         "Arcen Deck.app"
```

Example invocations:
```bash
# Local development build, signed but never notarized:
ARCEN_DEV_CODESIGN_IDENTITY="Apple Development: <Developer Name> (<APPLE_TEAM_ID>)" \
ARCEN_DEV_PROVISIONING_PROFILE=/path/to/dev.provisionprofile \
  packaging/macos/build-deck-app.sh --dev-sign

# Release build, notarized and stapled:
ARCEN_CODESIGN_IDENTITY="Developer ID Application: <Your Organization> (<APPLE_TEAM_ID>)" \
ARCEN_PROVISIONING_PROFILE=/path/to/deckarcentech.provisionprofile \
ARCEN_NOTARY_KEYCHAIN_PROFILE=<NOTARY_KEYCHAIN_PROFILE> \
  packaging/macos/build-deck-app.sh --release
```

## Entitlements and TCC — two different mechanisms

`packaging/macos/Deck.entitlements` requests only:
- `com.apple.application-identifier` / `com.apple.developer.team-identifier` —
  required whenever a provisioning profile is embedded; not a capability grant.
- `com.apple.developer.sustained-execution` — prevents CPU throttling while
  Deck is backgrounded during a live session. Profile-authorized (restricted
  entitlement, approved on the `deck.arcen.tech` App ID).
- `com.apple.developer.associated-domains` — `applinks:arcen.tech` /
  `webcredentials:arcen.tech`. Profile-authorized (wildcard `*`).

Deck is **not sandboxed** (no `com.apple.security.app-sandbox` key), so the
following App-Sandbox-only entitlements were removed as no-ops that the real
external profile does not authorize either:
- `com.apple.security.network.client` — meaningless outside App Sandbox; an
  unsandboxed app already has full outgoing network access for the QUIC
  connection to Pier.
- `com.apple.security.device.audio-input` — meaningless outside App Sandbox.
  Microphone capture is governed entirely by TCC: `NSMicrophoneUsageDescription`
  in `Deck-Info.plist` plus the explicit runtime `AVAudioEngine` permission
  check in `microphone.rs`, with no entitlement involved.
- `com.apple.security.device.input-monitoring` — not a documented Apple
  entitlement key at all; Input Monitoring is a TCC privacy permission
  (System Settings → Privacy & Security → Input Monitoring), not a
  code-signing entitlement. Default in-window AppKit tablet events
  (`NSEvent.tabletPoint`/`tabletProximity`) never require it. It would only be
  relevant to the quarantined, non-default experimental raw-HID/global-input
  path and must not be reintroduced for that path without a dedicated
  Release/Security review.

See `clients/macos/APPLE_ENTITLEMENT_REQUESTS.md` for the full rationale per
entitlement and for restricted-entitlement requests still pending with Apple.

---

## Notarization

One-time setup (run once per machine that performs `--release` builds):
```bash
xcrun notarytool store-credentials "<NOTARY_KEYCHAIN_PROFILE>" \
  --apple-id <APPLE_ID_EMAIL> \
  --team-id <APPLE_TEAM_ID>
# Paste an app-specific password from appleid.apple.com when prompted
```

Pass the resulting keychain profile name as `ARCEN_NOTARY_KEYCHAIN_PROFILE`.
Only `--release` notarizes, staples, and validates the staple; `--dev-sign`
and unsigned assembly never do.

The default Light build uses `packaging/macos/Deck.entitlements`. The
default-off `usb-hard-lab` physical-claim experiment uses the separately
reviewed `packaging/macos/Deck-usb-hard.entitlements`:

```bash
ARCEN_ENTITLEMENTS_FILE=packaging/macos/Deck-usb-hard.entitlements \
  packaging/macos/build-deck-app.sh --no-build --release
```

That file adds only `com.apple.developer.accessory-access.usb`; it does not
request the unrelated DriverKit/HID/HLS/network capabilities that the App ID
may authorize.

The active Developer ID profile is `<Deck trimmed-entitlements provisioning profile>`
(UUID `bc8a9ef2-a105-4d84-80fe-b9f3c5366c62`). It authorizes only the
application/team identity, Associated Domains, Sustained Execution, Claim USB
Accessory, and Apple's automatically generated team keychain-access group.
The obsolete Network Extension, Personal VPN, App Groups, System Extension,
Virtualization, DriverKit, HLS, Multipath, App Attest, Data Protection,
Background GPU Access, and increased-memory capabilities were removed from the
App ID before this profile was generated.

The previous `<Deck VM USB provisioning profile>` profile is retained only as
historical evidence. It did **not** contain `com.apple.vm.device-access`.
Adding that missing restricted entitlement to the executable produced a
successfully notarized app that macOS killed before `main` with
`Unsatisfied entitlements: com.apple.vm.device-access` and
`AppleMobileFileIntegrityError Code=-413 "No matching profile found"`.
Virtualization therefore does not substitute for the device-access grant.

DriverKit USB/HID grants do not appear in a Developer ID application profile;
they require a separate DriverKit extension identifier/profile. Neither
Virtualization nor VMNet is requested by Deck's binary.

The root-authorized libusb capture probe does not require any VM entitlement.
Non-root capture is **not** obtainable through a Developer ID profile at all:
Apple Developer Technical Support confirmed on 2026-08-20 (Case-ID 21584866)
that `com.apple.vm.device-access` is reserved for Mac App Store hypervisor apps
targeting systems before macOS 27, because only those apps cannot escalate
privileges. Developer ID builds must instead run a small separate helper tool
as root and reach it over IPC, or use Accessory Access on macOS 27 and later.
See `clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`.

---

## Restricted entitlements (future — requires Apple approval)

| Entitlement | For | Status |
|---|---|---|
| `com.apple.developer.hid.virtual.device` | macOS Pier — inject virtual Wacom via IOHIDUserDevice | Not yet requested |

See `clients/macos/APPLE_ENTITLEMENT_REQUESTS.md` for the justification document to
submit when requesting restricted entitlements.
