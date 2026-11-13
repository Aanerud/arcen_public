# ADR 0019: IOUSBHost is the USB backend on Apple platforms

**Status:** Proposed 2026-11-09. Step M1 (section 8) is implemented: the
backend trait and exporter engine in `arcen-usb-bridge`, the synthetic
backend with tests S1 and S2, and the helper on the trait, still on libusb.
M1's physical run on a tablet is still owed. The first half of M2 is
implemented: the `arcen-usb-iousbhost` crate with the `DeviceCapture` route
(section 4), linked into the helper only by its non-default `iousbhost`
feature. A helper built with that feature captures with it (below); since
0.19.0 every release helper is built with it (status note below).
Its real-tablet test is written and not yet run. Everything else is design.

**0.19.0 status note (2026-11-12):** the release Deck builds its helper with
`--features iousbhost` (`packaging/macos/build-deck-app.sh`; a release build
refuses a helper that does not link `IOUSBHost.framework`), so every 0.19.0
Deck captures with `DeviceCapture`; `--backend libusb` remains selectable in
the helper. A real Wacom Intuos5 has been captured with it, bridged to
Linux X11, Linux Wayland and macOS Piers, and handed back to macOS
(hand-back as an unplug and replug, section 4). The Intel Deck build uses
the same backend (Native tablet needs macOS 13 on the Deck) and has not been
proven with a physical tablet.

> Touches the backend boundary in `arcen-usb-bridge` (sections 2 and 3) and
> the dependency removal, entitlements, privilege model and App Store
> consequences (sections 1, 4, 5, 6 and 7). Builds on
> [ADR 0011](0011-macos-privileged-usb-helper.md) (privileged helper), which
> it amends but does not replace; [ADR 0017](0017-forward-only-pen-input.md)
> (no Native tablet on Windows hosts) is unchanged.

## Context

**Native tablet** is the USB bridge: the Deck takes a physical tablet away
from its Mac and forwards its raw USB traffic, and the Pier presents it on a
virtual USB controller so the host's own tablet driver binds it
([pen-tablet-input.md §5c](../operations/pen-tablet-input.md#5c-native-tablet-usb-bridged-with-any-tablet)).
Policy, identity and lifecycle already live in `shared/usb-bridge`
(`classify_pen_tablet`, `evaluate_profile`, `authorize_hard_usb_tablet`,
`BridgedDeviceGuard`, `InFlightLedger`, `AttachmentMachine`). The
physical-device I/O does not: it lives in `clients/macos/usb-helper`, on
**libusb**.

**libusb is in every release Deck.** `packaging/macos/build-deck-app.sh`
embeds `arcen-usb-helper` by default (`EMBED_USB_HELPER=1`) and adds the
`usb-hard-lab` Deck feature with it. Both pull `rusb` with
`features = ["vendored"]`, which statically links libusb 1.0
(LGPL-2.1-or-later). `deny.toml` records the clarification and
the publication manifest lists `libusb1-sys` under `known_copyleft` —
"a recorded store-distribution blocker". The LGPL's relinking requirement
and its bar on further restrictions are what the
[Store Exception](../../LICENSES/LicenseRef-Arcen-Store-Exception.txt) §6
warns about: a third-party LGPL component can prevent Store distribution of
the artefact that contains it, whatever Arcen's own licence permits.

**Apple ships the replacement.** `IOUSBHost.framework` (macOS 10.15+) is
Apple's user-space USB host API: `IOUSBHostDevice`, `IOUSBHostInterface`,
`IOUSBHostPipe`, control, interrupt, bulk and isochronous transfers with
completion blocks on a dispatch queue
([IOUSBHost](https://developer.apple.com/documentation/iousbhost)). It can
**capture** a device the HID stack already owns:
`IOUSBHostObjectInitOptionsDeviceCapture` "will terminate all clients and
drivers of the IOUSBHostDevice and associated IOUSBHostInterface clients
besides the caller. Upon destroy of the IOUSBHostDevice, the device will be
reset and drivers will be re-registered for matching". It requires root, or
`com.apple.vm.device-access` with `IOServiceAuthorize`
(`IOUSBHostDefinitions.h`;
[deviceCapture](https://developer.apple.com/documentation/iousbhost/iousbhostobjectinitoptions/devicecapture)).
`com.apple.vm.device-access` is reserved for Mac App Store hypervisor apps
(ADR 0011) and is not available to a Developer ID Deck, so on macOS 26 and
earlier capture means the root helper.

**macOS 27 adds Accessory Access**
([AccessoryAccess](https://developer.apple.com/documentation/accessoryaccess),
macOS 27.0+). An app registers interest with
`AAUSBAccessoryManager.registerListener(_:matchingCriteria:)`; the user
attaches a matching device to that app from a system menu extra;
`AAUSBAccessory.open(serviceQueue:)` then hands the app an
**`IOUSBHostDevice`** "for exclusive access". It needs
`com.apple.developer.accessory-access.usb`, which Arcen's Deck profile carries
(`packaging/macos/Deck-usb-hard.entitlements`), and no root. "People have
visibility into what apps are using their devices, and can attach and
detach devices at any time"
([WWDC26 session 224](https://developer.apple.com/videos/play/wwdc2026/224/)).
The I/O API after `open` is the same IOUSBHost API as the root path.

So one framework serves both acquisition routes, and libusb serves none
that IOUSBHost does not.

### What is not known yet

Apple does not publish which device types Accessory Access offers. Whether
it offers a **HID-class** tablet, and whether `open` takes it from
`AppleUserUSBHostHIDDevice` (Apple's user-space USB-HID driver, which binds
tablets on a Mac — `hosts/macos/SIGNING.md`) and from a tablet maker's
driver, is unverified. ADR 0011 already records this as the open question
for macOS 27. This ADR is written so that the answer changes *which
acquisition route ships*, not the backend.

## Decision

### 1. IOUSBHost replaces libusb for every Apple USB backend

All USB device I/O on macOS uses `IOUSBHost.framework`. `rusb`,
`libusb1-sys` and the vendored libusb leave the dependency graph of every
artefact, including the helper and lab builds, once the migration in
section 8 completes. No Apple product keeps a libusb backend.

Rust reaches it through `objc2-io-usb-host` (Zlib OR Apache-2.0 OR MIT,
generated from Apple's headers; it carries `DeviceCapture`,
`destroyWithOptions:`, `configureWithValue:matchInterfaces:error:` and the
asynchronous `enqueue…completionHandler:` calls), with `block2` and
`dispatch2` from the same project. Accessory Access has no published
bindings (`objc2-accessory-access` is a reserved 0.0.0 name); its four
classes are bound by hand with `objc2::extern_class!` and the framework is
**weak-linked**, because the Deck deploys to macOS 12.0
(`packaging/macos/Deck-Info.plist`). No C or Swift shim is added.

**Owner: Release/Security** (dependency and licence), **macOS Client**
(implementation).

### 2. The backend boundary: policy and the exporter engine in `shared/`, I/O in the platform

`shared/usb-bridge` stays OS-free, `#![forbid(unsafe_code)]` and
dependency-light. It gains two things, both portable:

1. **A backend trait** that names what a platform must provide — and only
   that. Sketch, not final API:

   ```rust
   /// One captured physical device. Dropping it must release the device
   /// and return it to the local OS, exactly like `release`.
   pub trait CapturedUsbDevice: Send {
       fn facts(&self) -> &CapturedDeviceFacts; // descriptors read after capture
       fn submit_control(&self, urb: UrbId, setup: SetupPacket, out: &[u8]) -> Result<(), BackendRefusal>;
       fn submit_interrupt_in(&self, urb: UrbId, endpoint: EndpointAddress, length: usize) -> Result<(), BackendRefusal>;
       fn submit_interrupt_out(&self, urb: UrbId, endpoint: EndpointAddress, data: &[u8]) -> Result<(), BackendRefusal>;
       fn cancel(&self, urb: UrbId);
       fn release(self: Box<Self>, reason: ReleaseReason) -> ReleaseOutcome;
   }

   /// Completions arrive on whatever thread the platform uses.
   pub trait CompletionSink: Send + Sync {
       fn complete(&self, urb: UrbId, status: BackendStatus, data: Vec<u8>);
   }
   ```

   `BackendStatus` is a closed portable vocabulary (`completed`, `stall`,
   `timed_out`, `cancelled`, `disconnected`, `protocol`, `io`) that shared
   code maps to `UrbStatus`; a platform never invents a `UrbStatus`.
   `ReleaseReason` names why the device goes back (`session_ended`,
   `peer_detached`, `requested_by_os` for `kUSBHostMessageDeviceIsRequestingClose`
   or an Accessory Access detach, `user_switch`, `unplugged`, `error`).
   Acquisition is deliberately **not** in the trait: capture-by-root and
   attach-by-user have different shapes, and each platform presents the
   result as a `CapturedUsbDevice`.

   *As implemented in M1* (`shared/usb-bridge/src/backend.rs`): each
   `submit_*` takes a `PendingCompletion`, which completes exactly once by
   construction (a dropped one completes as `io`); a backend that cannot
   start a transfer completes it with `PendingCompletion::refuse`. Control
   operations arrive as `ControlOperation` (`Transfer`,
   `SelectAlternateSetting`, `ClearHalt`). `cancel` is advisory. A backend
   reports an OS-initiated end through the exporter's `EndSignal`, and the
   release then uses that reason. `hold_until_report` is the interrupt hold
   for backends that can only poll.

2. **The exporter engine**, moved out of
   `clients/macos/usb-helper/src/capture.rs`: generation binding, URB
   admission against `MAX_IN_FLIGHT_URBS` and `MAX_TRANSFER_BYTES`,
   cancellation bookkeeping in `InFlightLedger`, the standard-request
   dispatch that is answered locally rather than sent to the device
   (`control_dispatch`), the "hold an interrupt IN until the device
   reports" rule that keeps an idle Linux host from resetting the device,
   and status mapping. It re-runs `evaluate_profile` and
   `authorize_hard_usb_tablet` on the descriptors the backend read itself,
   as ADR 0011 requires of the helper. It is generic over
   `CapturedUsbDevice`, so it runs unchanged in the root helper, in the
   Deck process, in a future non-Apple Deck, and in tests.

Platform backends, each a thin adapter:

| Backend | Where | Acquires by | Status |
| --- | --- | --- | --- |
| `IoUsbHostDevice` | new crate `clients/macos/usb-iousbhost` (`arcen-usb-iousbhost`) | `DeviceCapture` (root helper) **or** an `IOUSBHostDevice` from `AAUSBAccessory.open` | this ADR |
| `SyntheticDevice` | `shared/usb-bridge`, over the existing `SyntheticTabletDevice` | construction | this ADR; test-only |
| libusb (`rusb`) | helper, behind a lab-only feature, during migration only | — | removed by section 8 |
| `nusb` (usbfs, pure Rust) | a future Linux Deck | `detach_kernel_driver` + claim | not planned; recorded so nobody reaches for libusb |

Both the helper and the Deck link `arcen-usb-iousbhost`, so the capture
code exists once on macOS. The helper's minimal-root-surface rule
(ADR 0011 §1) holds: `objc2`, `objc2-foundation`, `objc2-io-usb-host`,
`block2` and `dispatch2` add no async runtime, transport, UI, media or
serialization framework, and they **remove** libusb's C from the root
process. Unsafe Objective-C calls stay inside `arcen-usb-iousbhost`, each
with a `SAFETY` comment.

The macOS Pier's future USB *target* (section 6) uses a different part of
the framework (`IOUSBHostControllerInterface`) and lives in `hosts/macos`.
If the Deck and Pier ever need the same Apple-only code, that is a
workspace-boundary question for Shared/Architecture (ADR 0001), not
something to solve by having one product depend on the other.

**No wire change.** The Deck still sends `arcen-protocol` URB frames and
`client_hello.usb_hard_device`; the Linux Pier and `usb-vhci` are untouched.

**Owner: Shared/Architecture.**

### 3. Two acquisition routes, one backend

| | Root helper route | Accessory Access route |
| --- | --- | --- |
| macOS | 12.0 – 26 (works on 27 too) | **27.0+** |
| Who opens the device | `arcen-usb-helper`, root via `SMAppService.daemon` (ADR 0011) | the Deck process itself, unprivileged |
| API | `IOUSBHostDevice initWithIOService:options:DeviceCapture` | `AAUSBAccessoryManager` → user attaches in the menu extra → `AAUSBAccessory.open` → `IOUSBHostDevice` |
| Entitlement | none (root) | `com.apple.developer.accessory-access.usb` (held) |
| User step | one administrator approval at helper registration | attach the tablet to Arcen Deck in the system's accessory menu |
| Developer ID Deck | yes | yes |
| Mac App Store Deck | **no** — guideline 2.4.5(v) forbids root escalation | yes, if gate G0 passes |

Selection at run time: on macOS 27+ with the entitlement present, the Deck
uses Accessory Access; otherwise a Developer ID Deck uses the helper when it
is registered; otherwise Native tablet is offered as unavailable with a
reason, and Tablet support stays available. A Mac App Store Deck never
contains the helper.

Accessory Access matching criteria are derived from shared policy: the
classified tablet's vendor and product, with an interface criterion for
HID class (`AAUSBAccessoryMatchingCriteria` with interface matching
dictionaries). An empty criteria array ("match any USB accessory") is never
used. The Deck must be a Dock app for the manager to present its UI; it is.

If gate G0 fails (Accessory Access does not offer or does not detach HID
tablets), the Accessory Access route is not built, the helper stays the only
macOS route, and a Mac App Store Deck ships without Native tablet. Nothing
else in this ADR changes.

**Owner: macOS Client** (routes), **Release/Security** (privilege model).

### 4. Capture and release lifecycle

```text
classify (registry only, nothing opened)
  → acquire (DeviceCapture | user attach + open)
  → configureWithValue(current, matchInterfaces: NO)   // HID drivers stay off
  → open each IOUSBHostInterface, copy interrupt pipes
  → verify descriptors against the authorisation (shared)
  → ACTIVE: URBs via enqueue…completionHandler on one serial queue
  → release: abort pipes → drain ledger (cancelled)
             → destroy device (reset + driver re-match), interfaces held open
             → old device seen terminated → hold the port empty 1.5 s
             → destroy interfaces → device re-enumerates (timeline logged)
```

Rules:

- **Classification never opens a device.** It reads `IOHIDDevice`
  `ReportDescriptor` from the I/O Registry, as `registry.rs` does today, so
  keyboards and mice are never touched.
- **The local tablet stops working while captured.** Both routes take the
  device from macOS: capture terminates its drivers; Accessory Access
  removes it from the host. The Deck says so before it captures, and shows
  a persistent indicator while it holds the device.
- **Every exit releases.** Session end, peer detach, Pier refusal,
  `BridgedDeviceGuard` violation, Deck quit, helper SIGTERM, and a `Drop`
  of the captured device all run the same release. Release is idempotent
  (`destroy` "multiple times has no effect"). `destroy` is never called from
  a completion handler — Apple documents that it deadlocks — so release is
  queued to a dedicated thread.
- **The hand-back looks like an unplug and replug.** Without a hold, the
  re-enumerated device arrives about 50 ms after the old one went away.
  Wacom's driver 6.4.9 was still handling the removal at that point. It
  logged `NotificationManager.cpp:206: failed assertion:
  _CallbackMap[obj_I] == nullptr` and dropped the arrival, and the tablet
  stayed dead on the Mac until its agent restarted. The kernel registers the
  new `IOUSBHostDevice` only once "the previous object has finished
  terminating" (`IOUSBHostDevice.h`, `reset`), and a client that still has
  its provider open defers that (`IOService::didTerminate`). The release
  therefore destroys the device while the capture's interface objects are
  still open. It holds them for 1.5 s after the old device leaves the
  registry, then lets them go. Nothing outside Arcen is touched. The helper
  logs the timeline (`hand-back: … terminated=+Nms hold_released=+Nms
  reenumerated=+Nms`) and warns when the device came back after less than
  1 s. If the old device never goes away, the release lets the interfaces go
  and resets the device again through a fresh capture. The decisions live in
  `usb-iousbhost/src/handback.rs`.
- **The OS can take it back.** On `kUSBHostMessageDeviceIsRequestingClose`
  the backend surrenders (`destroyWithOptions: DeviceSurrender`) and ends
  the bridge with `requested_by_os`; on an Accessory Access disconnect,
  including fast user switching and console logout (which Apple documents
  as disconnecting every accessory), it ends the bridge with
  `user_switch`. The Pier sees an ordinary unplug. A reconnect is a new
  attachment generation.
- **Crash safety.** A crash cannot run `destroy`. Apple documents what
  `destroy` does, not what a dead client's teardown does, so the design does
  not rely on it:
  - *Helper route:* before capture the helper writes a root-owned
    `captured` marker naming the device's registry ID; `launchd` already
    restarts the daemon after an abnormal exit (`KeepAlive` with
    `SuccessfulExit = false` in
    `packaging/macos/tech.arcen.deck.usbhelper.plist`), and on finding a
    marker the new instance re-opens that device
    with `DeviceCapture` and immediately calls `destroy` — the documented
    reset-and-rematch — then clears the marker. A Deck crash closes the
    helper's socket, which is already a release trigger.
  - *Accessory Access route:* the system owns the attachment; the user can
    release the device from the menu extra at any time, and Apple
    re-offers it on reconnection. The Deck clears nothing on its own.
  - Both are proved by test T-crash before this ADR is accepted. Unplugging
    the tablet always restores it.
- **Fail closed.** Any capture step that fails releases what was taken and
  reports a reason; the session continues on Tablet support.

*As implemented in M2, part one* (`clients/macos/usb-iousbhost`):

- **The `iousbhost` helper.** A helper built with `--features iousbhost`
  (every release helper since 0.19.0) captures with `IOUSBHost` by default
  (`--backend libusb` keeps the old path in the same binary); a helper built
  without the feature has libusb only. The tablet is chosen from the I/O
  Registry alone (`iousbhost::choose_tablet`: the libusb path's rules — not a
  pen tablet is skipped, a refused pen tablet is reported, absence is "no pen
  tablet is attached"), captured with `DeviceCapture` and bridged through the
  shared engine, which re-checks everything against what the device returns.
  The helper logs `capture backend: <name>` at start and the backend of every
  capture, and tells the Deck in a `TAG_INFO` frame (`backend=<name>`) sent
  right after the hello; the Deck logs it with the capture
  (`privileged USB helper captured the device for Hard USB … backend=`). A
  Deck that predates the frame skips it. The Deck↔Pier wire is unchanged.

- The target is chosen by registry ID; if that entry is gone, the same
  vendor and product at the same `locationID` is the same device after a
  re-enumeration. A registry ID that now names another identity or port is
  refused (`matching::select_target`).
- The HID report descriptors come from the registry before capture (capture
  terminates the drivers that publish them); the device and configuration
  descriptors are read back from the captured device, and a changed identity
  refuses the capture before the shared engine re-runs its checks.
- Control requests to an interface or endpoint recipient go through that
  interface's `IOUSBHostInterface` object, the rest through the device.
  `SET_INTERFACE` and `CLEAR_FEATURE(ENDPOINT_HALT)` use the interface and pipe
  calls on a helper thread.
- `IOUSBHost` aborts a whole pipe, so a cancellation aborts one only when every
  transfer pending on it was cancelled; otherwise the transfer finishes and the
  engine drops its late completion. Interrupt pipes take no completion
  timeout, so an interrupt OUT the device never accepts ends at cancellation
  or release.
- The marker is `/private/var/db/arcen/state/usb-helper.captured`. It is not
  under `/var/run`: on macOS that directory is `root:daemon` 0775 without the
  sticky bit, so a member of group `daemon` can rename, replace or delete any
  entry in it — a subdirectory included — between a check and a use. Every
  ancestor of `/private/var/db/arcen/state` is `root:wheel` 0755, `arcen` is
  root's 0755 and `state` is root's 0700; nothing on that path is writable by
  anyone but root. The helper creates the directories at start and verifies
  the whole chain before every use, from `/` down, opening each component with
  `O_DIRECTORY | O_NOFOLLOW` relative to its parent and checking owner and
  mode on the descriptor (`arcen-secure-dir`). Any other directory is refused;
  then no recovery runs and no `IOUSBHost` capture proceeds. The marker is
  then written, read and removed only relative to that pinned descriptor
  (`openat` with `O_EXCL | O_NOFOLLOW`, `renameat`, `unlinkat`), read with
  `O_NOFOLLOW | O_NONBLOCK` so neither a link nor a FIFO can redirect or block
  the root helper, and acted on only when the open descriptor is a regular
  root-owned 0600 file. `/private/var/db` survives a restart while registry
  IDs do not, so the marker records `kern.bootsessionuuid`, and a marker from
  another boot (or written when the boot cannot be read) is discarded without
  touching any device. Recovery resets only the exact registry entry the
  marker names, while it still has the recorded identity and port; a device
  that re-enumerated since, or is gone, is left alone, and the marker is
  removed either way. Residual risk: only root can create, swap, replay or
  remove the marker, so the remaining failure is a crash recovery that cannot
  run (for example a helper that never restarts); unplugging the tablet then
  restores it (test T-crash).
- A requesting-close (`kUSBHostMessageDeviceIsRequestingClose` or
  `kIOMessageServiceIsRequestingClose`) and a termination are recorded by the
  interest handler, reported to the engine at once, and acted on by the
  release thread: a surrender for the first, a plain destroy for the second.
  The first ending wins.

**Owner: macOS Client**, with Release/Security for the helper marker.

### 5. Entitlements, per product

**Arcen Deck (`deck.arcen.tech`).**

| Entitlement | State | Use |
| --- | --- | --- |
| `com.apple.developer.accessory-access.usb` | held | Accessory Access route, macOS 27+ (proposed, step M4; not implemented) |
| `com.apple.security.app-sandbox`, `com.apple.security.network.client` | self-service | Mac App Store variant only |
| `com.apple.security.device.usb` | self-service | Mac App Store variant, only if test E5 shows Accessory Access I/O needs it in the sandbox |
| `com.apple.vm.device-access` | not available to Developer ID apps (Mac App Store hypervisors only) | not used (ADR 0011) |
| helper | none | root via `SMAppService`, Developer ID only |

**Nothing needs to be requested from Apple for the macOS Deck.**

A future **iPad Deck** cannot use either route: the iPadOS SDK has neither
`IOUSBHost` nor `AccessoryAccess`. Its only USB host route is a DriverKit
`USBDriverKit` extension (iPadOS 16+, M-series iPads only), which would need
`com.apple.developer.driverkit`, `com.apple.developer.driverkit.transport.usb`
listing each supported tablet, and `com.apple.developer.driverkit.communicates-with-drivers`
on the app
([Creating drivers for iPadOS](https://developer.apple.com/documentation/driverkit/creating-drivers-for-ipados)).
Apple scopes the transport entitlement to the requester's own hardware vendor
ID ([Requesting entitlements for DriverKit development](https://developer.apple.com/documentation/driverkit/requesting-entitlements-for-driverkit-development)),
so a grant for other companies' tablets is unlikely. **Do not request these
until an iPad Deck exists**; an iPad Deck offers Tablet support, which needs
no USB.

**Arcen Pier (`pier.arcen.tech`).**

| Entitlement | State | Use |
| --- | --- | --- |
| `com.apple.developer.hid.virtual.device` | held | typed devices (ADR 0018), login window input; not USB |
| `com.apple.developer.usb.host-controller-interface` | **request** | `IOUSBHostControllerInterface`: a macOS Pier as a Native tablet *target*, and the synthetic USB test harness (section 9) |
| `com.apple.developer.accessory-access.usb` | not needed | a Pier never takes a local device |

`com.apple.developer.usb.host-controller-interface` is named in Apple's
header ("enables a process to instantiate a USB host controller to provide
access to remote USB devices or create synthetic USB devices",
`IOUSBHostControllerInterface.h`) and recorded as the Pier's prerequisite
in `hosts/macos/SIGNING.md`.

**Owner: Release/Security.**

### 6. The macOS Pier, if it needs USB

A macOS Pier becomes a Native tablet target through
`IOUSBHostControllerInterface`, the macOS analogue of `usb-vhci`: a
user-mode host controller on which the bridged device enumerates, so the
host's own driver binds it. It reuses the URB contract and
`BridgedDeviceGuard` unchanged. It is out of scope for this ADR's
implementation, and the entitlement it needs is not in the Pier's profile.

The macOS Pier serves Native tablet by another route instead:
[ADR 0022](0022-macos-pier-native-tablet-hid-adapter.md) decodes the bridged
tablet's reports for known models and presents them through a virtual HID
device (`IOHIDUserDevice`), which needs no host-controller entitlement.

### 7. Minimum OS

| Piece | Minimum |
| --- | --- |
| IOUSBHost device, interface, pipe, `DeviceCapture` | macOS 10.15 (Deck floor 12.0 is above it) |
| Root helper route | macOS 13.0 (`SMAppService` registration; a macOS 12 Deck offers Tablet support only) |
| Accessory Access route (proposed, not implemented) | macOS 27.0 |
| Mac App Store Native tablet | macOS 27.0 (and gate G0) |
| `IOUSBHostControllerInterface` (Pier target) | documented 10.15; to be confirmed on the oldest Pier OS before relying on it |
| iPad USB (DriverKit) | iPadOS 16, M-series |

### 8. Migration and rollback

| Step | Change | Rollback |
| --- | --- | --- |
| M1 | Backend trait and exporter engine in `shared/usb-bridge`; `SyntheticDevice` tests. The helper keeps `rusb`, adapted to the trait — a refactor with no behaviour change, proved by the existing helper tests and one physical run. | revert the commit |
| M2 | `arcen-usb-iousbhost` with the `DeviceCapture` route; the helper switches to it. `rusb` stays only behind a helper feature `lab-libusb`, off by default and rejected by release validation. | lab: build with `lab-libusb`. **Release: never** — a release that must roll back ships with `ARCEN_EMBED_USB_HELPER=0` (Native tablet unavailable, Tablet support unaffected), not with libusb. |
| M3 | After one release with the physical gate passed on IOUSBHost: delete `lab-libusb`, `rusb`, `libusb1-sys`, the `deny.toml` clarification, the `known_copyleft` entry, the libusb sections of `legal/THIRD_PARTY_NOTICES.md`; rewrite the `usb-capture-probe` lab subcommand on IOUSBHost. Release validation fails if `libusb1-sys` appears in any Deck or helper dependency tree. | git revert, lab only |
| M4 | Accessory Access route in the Deck (macOS 27+), if G0 passes. | runtime: the helper route remains on Developer ID builds |
| M5 | The helper remains necessary on systems without a qualified Accessory Access route; it retires only once that route has passed G0 on every supported macOS version. | keep it |
| M6 | Pier target via `IOUSBHostControllerInterface`, separate ADR. | — |

`nusb` (Apache-2.0 OR MIT, no libusb) stays for diagnostics; replacing it
with IOUSBHost registry reads is optional.

### 9. Test plan

**Without a tablet.** Synthetic tests require no tablet, no attached USB
device and no Accessory Access runtime.

| ID | Proves | How |
| --- | --- | --- |
| S1 | Exporter engine: admission, cancellation races, generation drain, interrupt hold, status mapping, bounds | `cargo test -p arcen-usb-bridge` with `SyntheticDevice` over `SyntheticTabletDevice`, any OS |
| S2 | Every exit path releases exactly once (drop, error, peer detach, requested-close, user switch, panic in the engine) | a fault-injecting test backend that counts releases |
| S3 | Bindings build for arm64 and x86_64 at deployment target 12.0; Accessory Access is weak-linked; the Deck launches on 26 and reports the route as unavailable | build, `otool -L`, launch |
| S4 | The helper with no device attached reports "no pen tablet is attached" | `arcen-usb-helper --probe` |
| S5 | No LGPL in any artefact | `cargo tree` for the Deck and helper shows no `libusb1-sys`; `cargo deny check licenses` passes without the clarification; `known_copyleft` empty; `validate_release_inputs.py` |
| S6 | A virtual HID device is not a capture candidate | an `IOHIDUserDevice` (Pier `hid.virtual.device` build) is ignored by the USB-only classifier |
| S7 | A host's driver binds a bridged Wacom and receives its pen (position, pressure, proximity, buttons) | the Deck's lab `--usb-emulate wacom-intuos5` feeds `SyntheticDevice<WacomIntuos5>` (an emulated Intuos5 with scripted strokes) through the shared exporter to a real Pier; `scripts/lab/wacom-replay/verify-pen.py` asserts the strokes at evdev |

A virtual HID device cannot test capture: it is an `IOHIDDevice` with no
`IOUSBHostDevice` beneath it, so neither IOUSBHost nor Accessory Access can
see it. Capture needs a USB device, which is what the next tier provides.

**With a synthetic USB device (no tablet).** Either
`IOUSBHostControllerInterface` presenting `SyntheticTabletDevice` from a
Pier-signed harness (needs the requested entitlement), or a small
microcontroller board running an open-source USB stack with the same
descriptors:

| ID | Proves |
| --- | --- |
| L1 | `DeviceCapture` terminates `AppleUserUSBHostHIDDevice`; `destroy` makes it re-match (`ioreg`) |
| L2 | URB round trip through the exporter engine, including interrupt hold and cancel |
| L3 | T-crash: `kill -9` the helper and the Deck during capture; the device is back with its driver within 5 s |
| L4 | Accessory Access on macOS 27: the device is offered, attached, opened, released from the menu extra, and re-offered after fast user switching (G0 for a HID device) |
| E0 | Whether `DeviceSeize` from an unprivileged process makes Apple's HID driver let go (a no-root route for 12–26) |
| E5 | Sandboxed Accessory Access, with and without `com.apple.security.device.usb` |

**With a real tablet.** Physical qualification requires a supported USB pen
tablet. P1: helper route on macOS 26 — capture,
local pointer stops, bridge to a Linux Pier, host driver binds Pen/Pad/Finger,
release restores local use. P2: every release trigger from section 4 plus
unplug, sleep/wake and T-crash. P3: Accessory Access route on macOS 27 (G0
with a tablet). P4: with and without the tablet maker's macOS driver
installed. P5: pressure and tilt in a host application, idle-then-resume,
reconnect with a transfer pending (the open items from ADR 0012). P6:
interrupt-to-URB latency p50/p99 on IOUSBHost no worse than libusb on the
same machine.

### 10. Risks

| Risk | Effect | Mitigation |
| --- | --- | --- |
| G0 fails: Accessory Access will not offer or detach a HID tablet | no Native tablet in a Mac App Store Deck; helper stays | decision 3 already degrades to that |
| Crash does not restore the device | tablet dead until replug | helper marker and restart; user release in the menu extra; T-crash gates acceptance |
| Accessory Access asks the user to attach each time | friction | measure in L4; document; it is Apple's consent model |
| `objc2-io-usb-host` is generated, pre-1.0, and runs in a root process | API churn; unsafe FFI | pin the version; confine `unsafe` to one crate; review |
| Two routes until the helper retires | double test matrix | shared engine; one backend; M5 retires one route |
| Weak-linking mistake | crash on macOS 12–26 at launch | S3 on 26; availability checks before any Accessory Access symbol |
| Apple refuses `usb.host-controller-interface` | no Pier target; no IOUSBHost loopback | microcontroller harness covers L1–L4 |
| iPad USB entitlement refused | no Native tablet on iPad | iPad Deck uses Tablet support |
| Behaviour differs from libusb (timeouts, stall handling) | regressions on the Linux Pier side | M1 lands first, so the engine is shared and only the backend changes; P1/P6 |

## Consequences

- No LGPL code in any Arcen artefact; `known_copyleft` becomes empty and the
  Store Exception's third-party caveat no longer bites the Deck.
- The root helper shrinks to policy plus Apple system frameworks.
- macOS 27 users can get Native tablet without an administrator prompt, and
  a Mac App Store Deck becomes possible — both subject to G0.
- `shared/usb-bridge` gains the exporter engine, so a second client needs a
  backend, not a second exporter.
- New `consumers.json` rows: `arcen-usb-iousbhost` as a consumer of
  `arcen-usb-bridge`, and new contracts on the existing helper and Deck
  rows.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Keep libusb, dynamically linked, to satisfy LGPL relinking | still LGPL in the bundle; the App Store restriction remains |
| `nusb` on macOS | cannot detach kernel drivers on macOS (its own documentation); it is the right answer only on Linux |
| Legacy IOUSBLib COM interfaces directly | what libusb wraps; older, C-COM, same privilege; no gain |
| DriverKit USB extension on macOS | System Extension approval, vendor-ID-scoped entitlement, C++ driver; IOUSBHost already does the job |
| `IOHIDManager` listening only | leaves the device with macOS; that is Tablet support's territory, not a USB bridge (ADR 0011 context) |
| `com.apple.vm.device-access` | reserved for Mac App Store hypervisors before macOS 27; not available to a Developer ID Deck |
