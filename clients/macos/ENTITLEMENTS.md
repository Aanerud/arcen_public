# macOS entitlements

What each Arcen app on macOS holds, why, and which capabilities Arcen does
not use. Signing setup: [`CERTIFICATES.md`](CERTIFICATES.md) (Deck) and
[`hosts/macos/SIGNING.md`](../../hosts/macos/SIGNING.md) (Pier).

## Which app holds which entitlement

The client (Deck) and the host (Pier) are separate App IDs with separate
provisioning profiles, and their entitlements serve opposite ends of a pen:
the Deck *takes* a tablet from its Mac, the Pier *publishes* devices on its own.

| App | Bundle ID | Entitlements file | Restricted entitlements it carries |
| --- | --- | --- | --- |
| **Arcen Deck** (client), default build | `deck.arcen.tech` | `packaging/macos/Deck-usb-hard.entitlements` | `com.apple.developer.sustained-execution`, `com.apple.developer.associated-domains`, `com.apple.developer.accessory-access.usb` (Native tablet, Hard USB) |
| Arcen Deck, `ARCEN_EMBED_USB_HELPER=0` | `deck.arcen.tech` | `packaging/macos/Deck.entitlements` | `com.apple.developer.sustained-execution`, `com.apple.developer.associated-domains` |
| Deck's embedded USB helper | (nested in the Deck) | none | none |
| **Arcen Pier** (host) | `pier.arcen.tech` | `packaging/macos/Pier-virtual-hid.entitlements` (`--with-virtual-hid`, every release) | `com.apple.developer.hid.virtual.device` |
| Arcen Agent Helper (host) | `pier.arcen.tech.agent` | none | none |
| Arcen Microphone driver (host) | `tech.arcen.microphone.driver` | none | none |

Every file that embeds a provisioning profile also carries
`com.apple.application-identifier` and `com.apple.developer.team-identifier`,
which a profile requires and which grant nothing. The Pier's profile also
authorizes three DriverKit HID entitlements (`Pier.entitlements`,
`--with-driverkit-hid`) that no release uses.

A requested entitlement must be authorized by the embedded provisioning
profile. Notarization does not change that: AMFI refuses to launch a binary
whose signature claims an entitlement its profile does not authorize.

## `com.apple.vm.device-access`

Not used. It is reserved for Mac App Store hypervisor apps supporting systems
before macOS 27, and every operation it gates also admits a privileged
process. The Deck instead captures a tablet in a privileged helper
([ADR 0011](../../docs/adr/0011-macos-privileged-usb-helper.md)) with
`IOUSBHost` ([ADR 0019](../../docs/adr/0019-iousbhost-usb-backend.md)).

## `com.apple.developer.hid.virtual.device`

**Held by the Arcen Pier** (`pier.arcen.tech`, `Pier-virtual-hid.entitlements`,
every release since 0.18.0). It lets the Pier's `hid-injector` child create
`IOHIDUserDevice` devices: the login window's virtual keyboard and pointer,
and the devices of a bridged Native tablet
([ADR 0022](../../docs/adr/0022-macos-pier-native-tablet-hid-adapter.md)).
A generic virtual pen digitizer reaches applications without pressure, tilt
or rotation, so Tablet support stays on CoreGraphics tablet events, and a
Native tablet's pen is read by the Pier for known models.

## `com.apple.developer.usb.host-controller-interface`

Not held, and not used. `IOUSBHostControllerInterface` (macOS SDK) lets a
process instantiate a USB host controller for remote or synthetic USB
devices; it needs this managed entitlement. The Pier serves Native tablet
through virtual HID devices instead (ADR 0022), and the Deck needs nothing
new: it captures a tablet through `IOUSBHost` in its root helper
(ADR 0011, macOS 13 or later). An Accessory Access capture route for
macOS 27 is proposed (ADR 0019, step M4) and **not implemented**; no Deck
captures through Accessory Access today.

## Listening to a tablet versus claiming it

`IOHIDManager` input-report listening needs only Input Monitoring, a grant
the user can give, and leaves the tablet working locally. An
`IOHIDManagerOpen` that fails with `0xe00002c5` (`kIOReturnExclusiveAccess`)
reflects a transient ownership state (another process holds the device), not
a structural bar. Claiming a device at the USB layer, which preserves its
real USB identity for the host's own driver, is what Native tablet's helper
does.

## Entitlements versus TCC — these are not the same mechanism

`Deck.entitlements` (a code-signing entitlement) and TCC (a per-user runtime privacy grant
recorded in `TCC.db`, requested via Info.plist usage-description strings or System Settings)
are separate Apple mechanisms. An app can need one, both, or neither for a given capability:

- **Microphone capture** needs *only* TCC: `NSMicrophoneUsageDescription` in
  `packaging/macos/Deck-Info.plist` triggers the consent prompt, and `microphone.rs` performs
  an explicit runtime `AVAudioEngine` permission check before capturing. Deck is not
  sandboxed, so `com.apple.security.device.audio-input` (an App-Sandbox-only entitlement)
  would be a no-op even if present, and is deliberately **not** in `Deck.entitlements`.
- **Input Monitoring** (System Settings → Privacy & Security → Input Monitoring) is *only*
  TCC. `com.apple.security.device.input-monitoring` is not a documented Apple entitlement key
  at all — it does not need to be, and must not be, present in `Deck.entitlements` for
  default in-window AppKit tablet capture (`NSEvent.tabletPoint`/`tabletProximity`), which
  never triggers this permission. It is relevant only to the quarantined, non-default
  experimental raw-HID/global-input path.
- **Outgoing network access** needs *neither* an entitlement nor TCC for an unsandboxed app.
  `com.apple.security.network.client` is an App-Sandbox-only entitlement and is deliberately
  **not** in `Deck.entitlements`.

None of the three keys above is authorized by the provisioning profile either, so leaving
them out also lets the provisioning-profile entitlement-superset validator
in `packaging/macos/validate_release_inputs.py` pass against the profile.

---

## Restricted entitlements in `Deck.entitlements`

| Capability | Entitlement key | Why |
|---|---|---|
| Sustained Execution | `com.apple.developer.sustained-execution` | Prevents macOS CPU throttling when Deck is backgrounded during a live remote session |
| Associated Domains | `com.apple.developer.associated-domains` | Reserved for `arcen.tech` universal links |
| Accessory Access (USB) | `com.apple.developer.accessory-access.usb` | Carried by the default (Native tablet) build for a proposed macOS 27 Accessory Access route (ADR 0019, M4), which is **not implemented**; Native tablet captures through the root helper |

Associated Domains has no URL-handling code in the Deck yet; the entitlement
is reserved, and the feature is not implemented.

## Capabilities not requested

The Deck requests only entitlements with an implemented use that its
profile authorizes. Increased Memory Limit, Background GPU Access, App
Attest, Data Protection, App Groups, Network Extension and keychain access
groups are not requested; adding one needs an implemented use and, where the
profile does not yet authorize it, a regenerated profile, or
`packaging/macos/validate_release_inputs.py` rejects the mismatch.
