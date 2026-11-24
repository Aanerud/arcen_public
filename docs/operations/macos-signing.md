# Signing and notarising the macOS Deck

For anyone who signs a macOS build. This page gives the procedure. It holds
no certificate name, notary profile name or provisioning profile identifier;
see [Keep the private values out of the repository](#keep-the-private-values-out-of-the-repository).

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

Every input comes from the environment; none is stored in the repository.

```sh
export ARCEN_CODESIGN_IDENTITY="Developer ID Application: <NAME> (<TEAM_ID>)"
export ARCEN_PROVISIONING_PROFILE="/path/to/<profile>.provisionprofile"
export ARCEN_NOTARY_KEYCHAIN_PROFILE="<notary-keychain-profile-name>"

packaging/macos/build-deck-app.sh --arch arm64 --release    # Arcen Deck.app
packaging/macos/build-deck-app.sh --arch x86_64 --release   # Arcen Deck Intel.app
```

Each run builds, signs, notarises and zips one app. The Intel app is
cross-compiled for `x86_64-apple-darwin` (`rustup target add x86_64-apple-darwin`
once) and uses the same bundle ID, identity and provisioning profile; see
[`packaging/macos/README.md`](../../packaging/macos/README.md#architecture-and-minimum-macos).

The script then, in order:

1. builds `arcen-deck-macos` release and verifies the governed Opus source;
2. assembles `Arcen Deck.app` (or `Arcen Deck Intel.app`), embedding
   `Info.plist`, the app icon, the third-party notices, and the provisioning
   profile;
3. validates the provisioning profile's CMS signature (it compiles
   `verify-provisioning-cms.c` with `xcrun clang` to do so, rather than trusting
   the profile blindly);
4. runs `codesign --sign` with hardened runtime and the entitlements file;
5. verifies with `codesign --verify --deep --strict`;
6. zips the bundle and submits it with `xcrun notarytool submit --wait`;
7. staples the ticket with `xcrun stapler staple` and validates it.

If step 6 or 7 is skipped, **the artefact is not releasable** — see the table
above.

### How the build checks the signing inputs

`build-deck-app.sh` takes the provisioning profile, signing identity and
notary credentials only through the environment variables above. It never
lists, logs or copies them into the repository. Before it decodes or embeds a
profile, it verifies the profile's CMS signature and Apple's provisioning
trust chain, with the checked-in Security.framework verifier; the exit status
of `security cms` alone is never taken as proof of the signer. It embeds the
profile before hardened-runtime signing.

In release mode it decodes the profile's metadata to temporary files, checks
the team and application identity, the expiry, the authorised entitlements
and the Developer ID Application certificate class, then deletes the
metadata. Notarisation, stapling and a Gatekeeper assessment of the artefact
are all required; none is optional in release mode.

Adversarial tests of the CMS check use an untrusted CMS fixture kept outside
the repository; they never generate or log profiles, certificates or keys.

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

The **Team ID and bundle ID are not secrets**. `codesign -d` reads them from
any signed copy of the Deck, so every release already publishes them. They
are also **build inputs**: these files must hold the real values.

| File | Must contain |
| --- | --- |
| `packaging/macos/Deck.entitlements` | real `<TEAM_ID>.<bundle-id>` and team identifier |
| `packaging/macos/Deck-usb-hard.entitlements` | same |
| `packaging/macos/Deck-Info.plist` | real `CFBundleIdentifier` |
| `packaging/macos/tech.arcen.deck.usbhelper.plist` | real helper bundle id |
| `packaging/macos/validate_release_inputs.py` | real `TEAM_ID` / `BUNDLE_ID` constants |

Do not replace them with placeholders. A placeholder still compiles and passes
the unit tests, but a `--release` build then fails metadata validation
(`provisioning profile team identifier does not match`) because the
entitlements no longer match the provisioning profile. Treat these files as
code, not prose.

What must stay out of the repository is anything that grants the ability to
*sign*: certificates, private keys and the notary keychain profile name.

## Keep the private values out of the repository

Nothing secret belongs in this repository. It is public, and
`scripts/ci/check-publication-hygiene.sh` fails the build on key material.

| Item | Where to keep it |
| --- | --- |
| Developer ID certificate + private key | your login keychain, with a secure backup |
| Apple Team ID, certificate common name | a secure store of your choice |
| App-specific password | the notary keychain profile (`xcrun notarytool store-credentials`) |
| Notary keychain profile name | a secure store of your choice |
| `.provisionprofile` files | a directory outside the repository |

`*.provisionprofile` (anywhere in the tree) is in `.gitignore`, but ignored is
not the same as absent: keep profiles out of the repository entirely.
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

An unsigned macOS build is fine for your own use. If you publish one, say so
in the release notes and give the workaround, or people will take the
download for broken:

```sh
xattr -dr com.apple.quarantine "/Applications/Arcen Deck.app"
```

Signed and notarised builds come from TUTTIFRUTTI AS; anyone may also build
the Deck from source.
