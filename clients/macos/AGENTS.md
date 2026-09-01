# macOS Client Ownership

**Owner role:** macOS Client

Own the macOS client UI boundary, media presentation, input capture, HID device
passthrough (HoIP), client session lifecycle, and macOS packaging coordination in
this path.

Validate on macOS with
`cargo build --locked --release -p arcen-deck-macos` and
`cargo test --locked -p arcen-deck-macos`, plus the root shared-crate test and
strict Clippy gates. There is no single-platform `--workspace` build.

## Streaming and presentation boundaries

- Production exposes exactly four complete presets: Auto, Speed, Grading, and
  HDR. Do not reintroduce independent performance/colour switches as the normal
  user surface; exact axes remain diagnostic/developer controls.
- The ordinary 8-bit UI/video path and the dedicated 10-bit Metal video layer
  are separate presentation pipelines. Do not force Auto/Speed through the
  wide layer or make Grading/HDR depend on the 8-bit egui surface.
- Transfer characteristics decide HDR. Ten-bit BT.709 Grading remains SDR;
  only a host-confirmed PQ stream enables the ITU-R BT.2100 PQ colour space,
  HDR metadata, and EDR.
- Preserve native VideoToolbox colour metadata, including PQ/HLG transfer
  constants. If native 10-bit presentation fails, retain the 8-bit fallback
  only with a persistent visible warning.
- Reconnect creates a fresh decoder/inbox and waits for a fresh keyframe.
  Teardown must discard queued frames rather than let stale data delay resume.

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
