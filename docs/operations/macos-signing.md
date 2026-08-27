# Signing and notarising the macOS Deck

Audience: whoever cuts a macOS release. This documents the **procedure**. It
deliberately contains **no certificate name, notary profile name, or
provisioning profile identifier** — see [Where the private values live](#where-the-private-values-live).

The Apple **Team ID** is deliberately *not* on that list. It is not a
credential: `codesign -dv` reads it from any signed application, so every copy
of the Deck already publishes it. It appears in `packaging/macos/*.entitlements`
and in the release validator because signing cannot work without it, and
removing it from the source while shipping it in the binary would be theatre
rather than protection. What must stay out of the repository is anything that
grants the ability to *sign*: certificates, private keys, and the notary
keychain profile name.

## Why this matters

An unsigned or ad-hoc-signed `Arcen Deck.app` runs fine on the machine that
built it and **fails on every other Mac**. The user sees:

> "Arcen Deck.app" is damaged and can't be opened. You should move it to the Trash.

That message is Gatekeeper refusing a quarantined bundle that is not notarised.
It is not a corrupted download, and no amount of re-downloading fixes it. A
macOS binary in a public release is therefore only useful if it is Developer ID
signed **and** notarised **and** stapled.

Check what you have before publishing anything:

```sh
codesign -dv "Arcen Deck.app" 2>&1 | grep -E 'Signature|Authority'
```

| Output | Meaning |
| --- | --- |
| `Signature=adhoc` | Local only. **Do not release.** |
| `Authority=Apple Development: ...` | Development identity. **Do not release.** |
| `Authority=Developer ID Application: ...` | Releasable, if also stapled (below). |

## Three signing modes

`packaging/macos/build-deck-app.sh` takes a mode flag:

| Flag | Result | Use for |
| --- | --- | --- |
| *(none)* | ad-hoc / linker-signed | local iteration on this machine only |
| `--dev-sign` | Apple Development identity, **not** notarised | testing entitlements on your own devices |
| `--release` | Developer ID + notarise + staple | anything another person will open |

## Release build

All five inputs come from the environment; none are stored in the repository.

```sh
export ARCEN_CODESIGN_IDENTITY="Developer ID Application: <NAME> (<TEAM_ID>)"
export ARCEN_PROVISIONING_PROFILE="/path/to/<profile>.provisionprofile"
export ARCEN_NOTARY_KEYCHAIN_PROFILE="<notary-keychain-profile-name>"

packaging/macos/build-deck-app.sh --release
```

The script then, in order:

1. builds `arcen-deck-macos` release and verifies the governed Opus source;
2. assembles `Arcen Deck.app`, embedding `Info.plist`, the app icon, the
   third-party notices, and the provisioning profile;
3. validates the provisioning profile's CMS signature (it compiles
   `verify-provisioning-cms.c` with `xcrun clang` to do so, rather than trusting
   the profile blindly);
4. runs `codesign --sign` with hardened runtime and the entitlements file;
5. verifies with `codesign --verify --deep --strict`;
6. zips the bundle and submits it with `xcrun notarytool submit --wait`;
7. staples the ticket with `xcrun stapler staple` and validates it.

If step 6 or 7 is skipped, **the artefact is not releasable** — see the table
above.

### One-time notary keychain profile

`notarytool` reads credentials from a named keychain profile so no secret ever
appears on a command line or in shell history. Create it once:

```sh
xcrun notarytool store-credentials "<notary-keychain-profile-name>" \
  --apple-id "<apple-id-email>" \
  --team-id "<TEAM_ID>" \
  --password "<app-specific-password>"
```

Generate the app-specific password at <https://appleid.apple.com> — it is not
your Apple ID password, and it can be revoked independently.

## Verifying a release artefact

Run these against the artefact you are about to publish, not the one you built:

```sh
codesign --verify --deep --strict --verbose=2 "Arcen Deck.app"
xcrun stapler validate "Arcen Deck.app"
spctl --assess --type execute --verbose "Arcen Deck.app"   # expect: accepted
```

The most realistic check is to download the published asset on a Mac that has
never seen the source tree, and open it.

## What must stay real in the repository

There is an important distinction between *secrets* and *identifiers*.

The **Team ID and bundle ID are not secrets**. They are embedded in every signed
binary you ship and anyone can read them out of a release artefact with
`codesign -d`. Hiding them buys nothing. More importantly, they are **functional
build inputs**: these files must contain the real values or the build fails.

| File | Must contain |
| --- | --- |
| `packaging/macos/Deck.entitlements` | real `<TEAM_ID>.<bundle-id>` and team identifier |
| `packaging/macos/Deck-usb-hard.entitlements` | same |
| `packaging/macos/Deck-Info.plist` | real `CFBundleIdentifier` |
| `packaging/macos/tech.arcen.deck.usbhelper.plist` | real helper bundle id |
| `packaging/macos/validate_release_inputs.py` | real `TEAM_ID` / `BUNDLE_ID` constants |

This is not theoretical. A documentation-scrub pass once replaced those values
with `TEAMID1234` / `com.example.arcen.deck` placeholders across all five files.
Everything still compiled and every unit test still passed, because the test
fixtures were rewritten consistently. It failed only at
`--release`, with:

```
release metadata validation failed: provisioning profile team identifier does not match
```

The entitlements no longer matched the provisioning profile, and the validator
was checking the real profile against fake constants. Treat these files as code,
not prose.

By contrast, the genuinely secret material below must never be committed.

## Where the private values live

Nothing secret belongs in this repository. It is public, and
`scripts/ci/check-publication-hygiene.sh` fails the build on key material.

| Item | Where it lives |
| --- | --- |
| Developer ID certificate + private key | login keychain (and a secure backup) |
| Apple Team ID, certificate common name | password manager |
| App-specific password | password manager; stored in the notary keychain profile |
| Notary keychain profile name | password manager |
| `.provisionprofile` files | outside the repository (for example an `AppleCerts/` directory alongside it) |
| Test-host inventory, lab addresses | `.claude/`, which is gitignored |

`clients/macos/*.provisionprofile` and `.claude/` are both in `.gitignore`.
Do not `git add -f` either.

Because those locations are deliberately outside version control, they are
**not backed up by git**. Make sure the certificate and its private key exist in
a second place; losing the private key means revoking and reissuing.

## Windows, for comparison

The Windows Pier and its credential provider need Authenticode signing, and the
credential provider is logon-path security-critical. That is a separate
procedure and is not covered here. An unsigned Windows installer triggers
SmartScreen rather than a hard refusal, so it is degraded but not blocked in
the way an un-notarised macOS bundle is.

## If you are not signing

Publishing an unsigned macOS build is a legitimate choice for a project with no
support commitment, but say so plainly in the release notes and tell people the
workaround, or they will reasonably assume the download is broken:

```sh
xattr -dr com.apple.quarantine "/Applications/Arcen Deck.app"
```

Better still, ship macOS as source-only and let people build it, which is what
the AGPL expects anyway.
