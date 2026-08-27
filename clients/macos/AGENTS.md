# macOS Client Ownership

**Owner role:** macOS Client

Own the macOS client UI boundary, media presentation, input capture, HID device
passthrough (HoIP), client session lifecycle, and macOS packaging coordination in
this path.

Validate on macOS with
`cargo build --locked --release -p arcen-deck-macos` and
`cargo test --locked -p arcen-deck-macos`, plus the root shared-crate test and
strict Clippy gates. There is no single-platform `--workspace` build.

## Signing and certificates

- Cert inventory and dev-machine setup: `clients/macos/CERTIFICATES.md`
- Apple entitlement request justifications: `clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`
- Build + sign: `packaging/macos/build-deck-app.sh`
- Entitlements plist: `packaging/macos/Deck.entitlements`

When adding new entitlements: update the plist, check the capability is enabled on the
`deck.arcen.tech` App ID at developer.apple.com, and update `CERTIFICATES.md`.

Escalate shared API or protocol changes to Shared/Architecture; keychain,
permissions, signing, notarization, packaging, and release changes to
Release/Security.
