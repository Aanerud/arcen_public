# Apple Entitlement Requests

This document captures the justification text to submit when requesting restricted
entitlements from Apple. Each section corresponds to one entitlement request via
developer.apple.com → Contact → Request an Entitlement.

---

## `com.apple.vm.device-access`

**Status: closed 2026-08-20 — not applicable to Arcen, and not obtainable.**
Apple Developer Technical Support (Case-ID 21584866) answered the request
directly: this entitlement is intended only for hypervisor apps with custom USB
accessory integration that ship on the **Mac App Store** and support systems
before macOS 27. It exists solely because Mac App Store apps are not allowed to
escalate privileges. Every operation it gates also admits a privileged process,
so a Developer-ID-signed app neither needs it nor qualifies for it.

Do not re-request this entitlement, and do not record it as a blocker anywhere.
The remaining work is Arcen's own privileged-helper design, not an Apple
authorization gate.

### Apple's directed path for Arcen

1. **Now, under Developer ID:** split the privileged USB capture code into a
   small, separate helper tool, run **only that helper** as root, and drive it
   from Deck over IPC. Running the whole app as root is explicitly rejected by
   Apple and by this repository.
2. **macOS 27 and later:** replace the helper with the new
   [Accessory Access framework](https://developer.apple.com/documentation/AccessoryAccess),
   which removes both the entitlement and the root requirement. macOS 27 is in
   beta as of this writing, so the helper remains required for supported
   releases.

Apple also confirmed that Virtualization framework's USB accessory integration
is itself built on Accessory Access. Virtualization was therefore never a route
to device access, which is exactly what the failed 2026-08-13 pairing
experiment measured.

Design reference for the escalation and IPC boundary: Apple Developer Forums,
["BSD Privilege Escalation on macOS"](https://developer.apple.com/forums/thread/708765).
Apple offered follow-up support in the
[Processes & Concurrency](https://developer.apple.com/forums/topics/app-and-system-services/processes-and-concurrency)
forum subtopic.

Helper packaging, signing, notarization, and the privilege/IPC trust boundary
are Release/Security decisions; the helper's capture behavior is macOS Client's.
The design is written up in
[`../../docs/todo/macos-privileged-usb-helper.md`](../../docs/todo/macos-privileged-usb-helper.md).

### Virtualization pairing experiment (2026-08-13)

After Virtualization was enabled on the App ID, the then-current Developer ID
profile was:

- Name: `<Deck VM USB provisioning profile>`
- UUID: `774dbd47-ac84-402a-94d0-8985e5c3fdf9`

Its entitlement dictionary includes:

```xml
<key>com.apple.security.virtualization</key>
<true/>
```

It does not separately list `com.apple.vm.device-access`. Following Apple's
support direction, a lab build requested both keys. Signing, notarization,
stapling, and Gatekeeper assessment all succeeded, but macOS killed the binary
before `main`. Unified logging reported:

```text
Unsatisfied entitlements: com.apple.vm.device-access
Restricted entitlements not validated, bailing out.
AppleMobileFileIntegrityError Code=-413 "No matching profile found"
```

The experiment was removed immediately and the fail-closed profile-entitlement
validator remains unchanged. Apple's 2026-08-20 answer explains the result:
Virtualization does not carry device access because Virtualization framework's
own USB accessory integration is built on Accessory Access, and the
device-access entitlement is reserved for Mac App Store hypervisors.

The App ID was subsequently trimmed and the active profile is now
`<Deck trimmed-entitlements provisioning profile>` (UUID
`bc8a9ef2-a105-4d84-80fe-b9f3c5366c62`). It contains neither Virtualization
nor `com.apple.vm.device-access`; the failed pairing is retained as historical
evidence only. There is no longer an open entitlement request.

### `Claim USB Accessory` experiment (2026-08-13)

The `com.example.arcen.deck` App ID now authorizes Apple's self-service
**Claim USB Accessory** capability. A regenerated Developer ID profile
(`<Deck USB provisioning profile>`, UUID
`7d0f9190-c032-405a-8b21-254970eca91e`) contains:

```xml
<key>com.apple.developer.accessory-access.usb</key>
<true/>
```

The initial lab-only `Deck-usb-hard.entitlements` requested that one additional
grant. The resulting Deck was Developer-ID signed, notarized, stapled, and
verified.

Measured against the attached Intuos5 touch L:

```text
device_open=ok
interface=0 class=03/00/00 claim=failed: exclusive access
interface=1 class=03/00/00 claim=failed: exclusive access
interface=2 class=03/01/02 claim=failed: exclusive access
```

So the entitlement changes the device-level access boundary, but **does not
make nusb detach Apple's HID ownership**. It is intended for the new
Accessory Access framework, not as a magic permission for legacy
`IOUSBInterfaceInterface::USBInterfaceOpen`.

This machine runs macOS 26.6.1 with SDK 26.5 and has no
`AccessoryAccess.framework` in either the SDK or runtime. The framework/API
cannot be implemented or tested here until an SDK and OS containing it are
available. Do not infer failure of the future Accessory Access route from the
nusb interface-claim result; they are different APIs.

The physical lab exporter uses public libusb through `rusb` and its macOS
whole-device capture path (`USBDeviceReEnumerate` with the capture bit). Apple's SDK header
states that operation requires either `com.apple.vm.device-access` plus
`IOServiceAuthorize`, or root. Apple's 2026-08-20 answer confirms the root
branch is the intended one for Developer ID distribution. The current lab Deck
is run as root as a whole app, which is the part that must change; it includes
explicit interface-release and driver-restoration guards.

That root route was proven on 2026-08-13: capture succeeded, all three Wacom
interfaces were claimed, all were released, and the Wacom/Apple HID stack
reclaimed the device afterward. Root capture itself requires neither
Virtualization nor VMNet. The attempted non-root Virtualization pairing failed
AMFI validation because no Developer ID profile can carry
`com.apple.vm.device-access`; VMNet remains unrelated and unrequested.

The trimmed active profile no longer authorizes System Extension or DriverKit.
A future USB DriverKit extension would require its own extension identifier and
DriverKit provisioning profile.

Measured on 2026-08-12: the existing `IOHIDManager` path opens a Wacom tablet with
**no entitlement at all**, with the vendor driver installed and running. That much is
proven. What it does *not* prove is that HID capture is sufficient, because the Linux
host cannot rebuild a Wacom from HID reports at all — `wacom.ko` is a USB driver and a
`/dev/uhid` device never binds it. The working architecture is a virtual USB host
controller (see `docs/operations/pen-tablet-input.md` §5a), and a virtual USB device
must answer descriptors, control transfers and URBs, which HID input reports are not.

The first 2026-08-13 lab implementation used synthesis deliberately to prove
shared URBs, QUIC, cancellation, enumeration, reports, and teardown. The next
tranche replaced that responder with root-authorized physical libusb capture.
pier-linux.example.internal enumerated the real full-speed `056a:0317`, bound `wacom.ko`, and
created native Pen, Pad, and Finger inputs. The final pending-interrupt
correction awaits a physical pen-report retest. Delivering this same exporter
without running the whole app as root now requires Arcen's own signed,
notarized privileged helper plus its IPC boundary — not an Apple grant.

Note this is a *different* entitlement from `com.apple.developer.hid.virtual.device`
below, and the two serve opposite ends of the same feature: this one would let the
**client** take a tablet away from the Mac, that one lets a macOS **host** publish a
virtual one. It is also not in the current external provisioning profile — decoding
`~/.arcen-signing` (`security cms -D`, non-secret metadata only) shows no device, USB
or HID entitlement of any kind — and it is not in the App ID capability list, because
Apple issues it only on request.

### What was measured, including the part that changed the conclusion

An Intuos5 touch L (`056a:0317`) publishes three collections: `0xFF0D` (vendor pen
data, where the pen reports arrive), `0xFF00` (vendor control) and `0x0001` (Generic
Desktop mouse). With Input Monitoring granted:

| when | matching | `IOHIDManagerOpen` |
| --- | --- | --- |
| driver running, long-lived | any combination, including a single collection | `0xe00002c5` fail |
| after the driver was stopped and restarted | vendor id alone (all three collections) | success |
| after the driver was stopped and restarted | vendor × `{0x0D, 0xFF0D}` | success |
| after the driver was stopped and restarted | `0xFF00` alone | success |

Per-collection `IOHIDDeviceOpen` in the last state: all three succeed.

`0xe00002c5` is `kIOReturnExclusiveAccess`, and `kIOHIDOptionsTypeSeizeDevice` does
not change it. **It is a transient ownership state, not a structural bar.** Some
process held the device; restarting the vendor driver made it let go, and after that
every matching combination opened — with the vendor driver running normally
throughout.

**Correction, recorded deliberately.** An earlier revision of this document, and the
first commit of the matching change, concluded that `0xFF00` was permanently held and
that quitting the Wacom driver was a prerequisite. Both are wrong, and both came from
reasoning across probes taken at different times without re-running the control. The
control was re-run and the old matching, which had failed, then passed. A single-shot
open is not evidence about permanent claimability.

### Why the reference needs it and Arcen does not

A reference remote-desktop product claims the device at the **USB** layer: its framework
contains `IOUSBHostDevice`/`IOUSBHostInterface`/`IOUSBHostPipe` and **zero** `IOHID*`
symbols, and its bundle carries `com.apple.vm.device-access`. Claiming a device that
way requires the entitlement, and takes the tablet from the Mac — which is why its own
guide says bridged mode "temporarily disconnects the tablet from local Mac
applications".

Arcen *listens* to input reports through `IOHIDManager`, which requires only Input
Monitoring, a checkbox the user can grant themselves, and leaves the tablet working
locally. For the synthetic lab tablet, AppKit capture is sufficient because Arcen owns
the synthetic descriptors and standard-request responder. It is not equivalent
to native Wacom passthrough; `/dev/uhid` cannot make `wacom.ko` bind.

### If this is ever revisited

The USB layer is required when the product must preserve the physical device's
real USB identity and native host-driver behavior. Apple authorization is no
longer a gate on that phase. It now depends on a privileged helper with a
reviewed IPC boundary, exclusive-ownership/restoration proof, and
Release/Security approval, with Accessory Access replacing the helper on
macOS 27 and later.

---

## `com.apple.developer.hid.virtual.device`

**Target app:** Arcen Pier (macOS host, not Deck)
**Bundle ID:** (TBD — macOS Pier not yet packaged; will be `pier.arcen.tech` or similar)
**Status:** Not yet submitted. Submit when macOS Pier is being packaged for distribution.

### Use case description (submit this text to Apple)

Arcen Pier is a remote desktop host application for macOS, allowing users to access
their Mac workstation from another device over an encrypted WebSocket connection.

We are implementing HID device passthrough ("HoIP" — HID over IP): a connected client
(Arcen Deck) reads the raw USB HID report descriptor and input reports from a physically
attached Wacom pen tablet and forwards them over the session wire. The macOS Pier host
reconstructs a virtual HID device using `IOHIDUserDeviceCreate` and injects the forwarded
reports via `IOHIDUserDeviceHandleReport`. This makes the remote pen tablet appear as a
locally-attached device to macOS applications such as Procreate, Affinity Designer, and
Pixelmator Pro, preserving full pressure, tilt, and rotation data.

This is the same technique used by commercial remote desktop products to support professional artist workflows over remote connections.

The `com.apple.developer.hid.virtual.device` entitlement is required by `IOHIDUserDevice`
(IOKit framework) to create virtual input devices in userspace without a kernel extension.
No DriverKit driver is involved.

**We do not use this entitlement for any purpose other than reconstructing forwarded pen
tablet input in an active remote desktop session.**

**Status note (this repo):** the client-side raw-HID capture path this request depends on
is currently quarantined (feature/runtime-gated, not active by default) while local
AppKit-based tablet termination is implemented instead. Do not submit this request until
the true-USB-bridge feature it supports is scheduled and Release/Security-approved.

---

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
  experimental raw-HID/global-input path, and reintroducing it for that path requires a
  dedicated Release/Security review, not a routine entitlement change.
- **Outgoing network access** needs *neither* an entitlement nor TCC for an unsandboxed app.
  `com.apple.security.network.client` is an App-Sandbox-only entitlement and is deliberately
  **not** in `Deck.entitlements`.

None of the three keys above are authorized by the real external provisioning profile
either, so removing them also lets the provisioning-profile entitlement-superset validator
in `packaging/macos/validate_release_inputs.py` pass against the actual profile.

---

## Restricted entitlements currently in `Deck.entitlements`

These were enabled on the `com.example.arcen.deck` App ID on 2026-07-21, are present in the real
external provisioning profile, and are the only restricted capability keys the app
currently requests:

| Capability | Entitlement key | Why |
|---|---|---|
| Sustained Execution | `com.apple.developer.sustained-execution` | Prevents macOS CPU throttling when Deck is backgrounded during a live remote session |
| Associated Domains | `com.apple.developer.associated-domains` | Reserved for `arcen.tech` universal links / web-credentials handoff from the web dashboard |

Associated Domains has no corresponding `CFBundleURLTypes` declaration or URL-handling code
in Deck yet, and no `apple-app-site-association` file is published at
`https://arcen.tech/.well-known/` yet — the entitlement is authorized and reserved, but the
feature itself is not implemented. Do not build product messaging around universal links
until that work lands.

## Other capabilities enabled on the App ID but not yet requested as entitlements

These capabilities were enabled on `com.example.arcen.deck` at developer.apple.com. Whether adding
the entitlement key today would pass validation against the real external profile depends on
which group below they fall into.

### Not yet authorized by the current external provisioning profile

These are **not** in the current external provisioning profile and **not** in
`Deck.entitlements`. Adding the entitlement key without first regenerating the profile to
include it will make the real profile fail entitlement-superset validation again:

| Capability | Entitlement key | Why (when implemented) |
|---|---|---|
| Increased Memory Limit | `com.apple.developer.kernel.increased-memory-limit` | 4K H.265 frame buffers exceed default cap under memory pressure |
| Background GPU Access | `com.apple.developer.gpu-access-to-background-tasks` | WGPU renderer must run continuously even when Deck is not the front window |
| App Attest | `com.apple.developer.devicecheck.appattest-environment` | Pier can cryptographically verify incoming connections are from legitimate Deck builds |
| Data Protection | `com.apple.developer.default-data-protection` | Encrypts stored session tokens and credentials at rest via Secure Enclave |

Add the entitlement key to `Deck.entitlements` only once: the capability has an implemented
use, and a regenerated provisioning profile from developer.apple.com actually authorizes it
— otherwise the release validator will correctly reject the mismatch.

### Already authorized by the current external provisioning profile, but unused

Unlike the group above, the real external profile **already authorizes** these — decoding it
(`security cms -D`, non-secret metadata only) shows `com.apple.developer.networking.networkextension`,
`com.apple.developer.networking.vpn.api`, `com.apple.security.application-groups`, and
`keychain-access-groups` present in its `Entitlements` dictionary today. They are deliberately
**not** requested in `Deck.entitlements` because Deck has no Network Extension bundle, no App
Group usage, and no shared keychain-access-group use yet — requesting an authorized-but-unused
entitlement would still pass validation but adds dead capability surface with no product
behavior, which the codebase's "retain only implemented, profile-authorized entitlements"
principle argues against.

| Capability | Entitlement key | Why (when implemented) |
|---|---|---|
| Network Extensions | `com.apple.developer.networking.networkextension` | Reserved for future Span (gateway) native macOS implementation |
| VPN API | `com.apple.developer.networking.vpn.api` | Would accompany a Network Extension implementation, not requested standalone |
| App Groups | `com.apple.security.application-groups` | No shared-container use between Deck and Pier exists yet |

Add the entitlement key to `Deck.entitlements` once Deck actually implements the corresponding
feature — no profile regeneration is required for these, since the profile already authorizes
them, but the entitlement should still not be requested until there is real behavior behind it.
