# macOS privileged USB helper for Hard USB

**Status:** design proposal, written 2026-08-20. No implementation yet.
**Owners:** macOS Client (helper behavior and capture), Release/Security (blessing
mechanism, signing, notarization, and the privilege/IPC trust boundary),
Shared/Architecture (the reused URB contract and the divergence from the fused
multicall convention).

## Why this document exists

The Hard USB lab path works, but only because the whole Deck app is launched as
root. That is a lab shortcut, not a shippable model, and it is the single
remaining gate between the proven physical exporter and a Hard USB feature that
an ordinary notarized Deck can offer.

Apple Developer Technical Support closed the alternative on 2026-08-20 (Case-ID
21584866): `com.apple.vm.device-access` is reserved for Mac App Store hypervisor
apps supporting systems before macOS 27, it exists solely because those apps may
not escalate privileges, and every operation it gates also admits a privileged
process. Developer ID distribution therefore neither needs nor can obtain it.
Apple's directed replacement is exactly what this document designs: split the
privileged code into a separate helper tool, run only that as root, and reach it
over IPC.

Background and current measured state:

- [`../operations/hard-usb-lab.md`](../operations/hard-usb-lab.md) — what is
  installed, what was measured, and the pending physical retest
- [`./input-only-usb-bridging.md`](./input-only-usb-bridging.md) — scope
  boundary, threat model, and the Light/Hard split
- [`../../clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`](../../clients/macos/APPLE_ENTITLEMENT_REQUESTS.md)
  — the closed entitlement question

## Outcome we want

A Developer-ID-signed, notarized, **non-root** Arcen Deck can capture one exact
host-approved USB input device and forward its real control and interrupt
traffic, with the privileged surface reduced to a small auditable helper, and
with device ownership deterministically restored on every exit path.

Non-goals: general USB forwarding, widening the v1 transfer envelope, changing
the Light typed-input default, or shipping Hard USB on by default.

## Recommended design

### 1. Blessing: `SMAppService.daemon(plistName:)`

`SMAppService` (macOS 13+) is Apple's current mechanism and the only one
recommended for new code.

| Mechanism | Verdict |
| --- | --- |
| `SMAppService.daemon(plistName:)` | **Use this.** Self-contained in the bundle, removed when the app is deleted, surfaces in System Settings → Login Items |
| `SMJobBless` | Deprecated in favor of `SMAppService` as of macOS 13. Only relevant if a macOS 12 floor is ever required |
| `AuthorizationExecuteWithPrivileges` | **Never.** Deprecated since macOS 10.7; runs an arbitrary binary as root with no signature check and a TOCTOU window |
| `.pkg` postinstall + `launchctl bootstrap` | Accepted fallback, but scatters files outside the bundle and needs its own uninstall path. Only if enterprise deployment demands it |

Proposed bundle layout, extending what `packaging/macos/build-deck-app.sh`
already assembles:

```text
Arcen Deck.app/
  Contents/
    Info.plist
    embedded.provisionprofile
    MacOS/arcen-deck
    Resources/com.example.arcen.deck.usb-helper                      <- new, root daemon
    Library/LaunchDaemons/com.example.arcen.deck.usb-helper.plist    <- new
```

The daemon plist uses `BundleProgram` with a bundle-relative path, and publishes
one Mach service:

```xml
<key>Label</key>            <string>com.example.arcen.deck.usb-helper</string>
<key>BundleProgram</key>    <string>Contents/Resources/com.example.arcen.deck.usb-helper</string>
<key>MachServices</key>     <dict><key>com.example.arcen.deck.usb-helper</key><true/></dict>
```

Registering a **daemon** (not an agent) prompts for administrator authentication
once. `SMAppService.Status` must be surfaced honestly in Deck's UI:
`requiresApproval` has to route the user to
`SMAppService.openSystemSettingsLoginItems()` rather than presenting Hard USB as
broken. Deck must degrade to Light, not fail, when the helper is absent.

### 2. A separate minimal binary, deliberately diverging from the Linux convention

[`../../hosts/linux/AGENTS.md`](../../hosts/linux/AGENTS.md) requires the Linux
helper to be a multicall subcommand of the single fused `arcen-pier` binary, and
`hosts/linux/src/usb_bridge/mod.rs` spawns it through `current_exe()`. **This
design deliberately does not mirror that on macOS**, and that divergence needs
Shared/Architecture sign-off.

The reason is the threat model, not convenience. On Linux the fused binary is
spawned by an already-privileged service, so fusion costs nothing. On macOS the
helper *is* the privilege boundary and runs as root. Fusing it into `arcen-deck`
would put the entire GUI, wgpu/Metal renderer, video decoder, QUIC transport, and
settings surface inside a root process image. A separate binary containing only
the XPC listener, the policy check, and the libusb capture loop is the smallest
defensible root surface, and is precisely what Apple's guidance asks for.

The helper must not link the client's UI, transport, or media code.

To be clear about what is being chosen: fusion is *mechanically available* on
macOS — see open question 4 — so this is a deliberate decision to decline a
capability we have, not a workaround for a platform limitation.

### 3. IPC: XPC Mach service carrying the existing URB frames

Use an XPC Mach service, not a Unix domain socket. The socket route would require
hand-rolling peer authentication, which is the exact part that must not be
hand-rolled.

The payload should **not** be a new protocol. `arcen-protocol` already defines
normalized URB submit/complete frames that the Linux helper carries today, with
bounds already fixed in `arcen-usb-bridge`:

- `MAX_TRANSFER_BYTES` = 16 KiB
- `MAX_IN_FLIGHT_URBS` = 128
- `MAX_CONFIGURATION_DESCRIPTOR_BYTES` = 4 KiB

Reusing those frames as the XPC message body means the macOS privilege boundary
and the Linux helper boundary validate the same shapes with the same limits, and
the existing bounds tests keep their meaning. XPC messages comfortably exceed
16 KiB, so the v1 envelope needs no fragmentation.

The connection's `interruptionHandler` and `invalidationHandler` are the helper
crash signal. A crash must fail the attachment closed and force a new generation;
it must never silently resume against a stale `AttachmentGeneration`.

### 4. Authenticating the caller — the part that must not be got wrong

A root daemon publishing a Mach service is a local privilege-escalation primitive
unless every connection is pinned to Deck's code signature.

- Primary: `NSXPCListener.setConnectionCodeSigningRequirement(_:)`, set **before**
  `resume()`, with a requirement of the form
  `identifier "com.example.arcen.deck" and anchor apple generic and certificate leaf[subject.OU] = "<APPLE_TEAM_ID>"`.
- Defense in depth: inside `listener(_:shouldAcceptNewConnection:)`, take
  `xpc_connection_get_audit_token`, build `kSecGuestAttributeAudit`, and validate
  via `SecCodeCopyGuestWithAttributes` plus `SecCodeCheckValidity` against the
  same requirement.
- Deck should symmetrically pin the helper with
  `NSXPCConnection.setCodeSigningRequirement(_:)`.

Hard rules:

- **Never** authenticate with `xpc_connection_get_pid()`. PIDs are reused, and
  that is the CVE-2019-8526 class of bug: the caller exits and a privileged
  process inherits its PID before the check completes. Audit tokens are unique for
  a process lifetime.
- Validate synchronously in the accept callback. Never cache a token for later
  asynchronous validation, and re-validate every new connection.
- `SMAuthorizedClients` / `SMPrivilegedExecutables` are `SMJobBless` keys and are
  not part of this design.

### 5. The helper is authoritative over policy

Deck must be treated as untrusted by the helper, exactly as the Pier already
treats Deck. The root helper must independently run
`arcen_usb_bridge::evaluate_profile` against descriptors it read itself, and must
refuse any device that Deck merely *claims* is approved. A compromised Deck must
not be able to talk the root helper into capturing an arbitrary device.

The helper additionally owns:

- exact identity/revision/interface matching before any claim;
- the permanently prohibited class list;
- capture, claim, release, and kernel-driver reattachment on **every** exit path,
  including panic and connection loss;
- bounded in-flight accounting and cancellation tombstones;
- refusing to run if it was started by anything other than launchd.

### 6. Signing and packaging changes

`packaging/macos/build-deck-app.sh` currently signs with `--deep`. That must
change: `--deep` applies one entitlement set uniformly to nested binaries and is
long-discouraged by Apple. Sign inside-out instead —

1. helper binary, `--options runtime --timestamp`;
2. main app binary;
3. outer `.app`.

Then notarize the whole bundle as one submission with `notarytool` and staple.
Both app and helper stay unsandboxed; the App Sandbox is incompatible with
whole-device USB capture. No provisioning profile is needed for the helper itself
under Developer ID, and no additional entitlement appears to be required for a
root daemon doing IOKit USB work — both points to be confirmed against a real
signed build before any release claim.

`packaging/macos/validate_release_inputs.py` will need to learn about the nested
helper so release validation does not silently ignore it.

### 7. Rust implementation shape

- Registration from Deck: `smappservice-rs`, or `objc2-service-management`
  directly if the thinner binding is preferred.
- Helper XPC listener: there is no maintained high-level XPC crate. The lowest
  risk shape is a small Objective-C shim (roughly 100–200 lines) owning listener
  setup and the code-signing check, calling into Rust for everything else.
  `xpc-connection-rs` is not currently maintained enough to sit on a root
  privilege boundary.
- Code-signing validation: `security-framework-sys` plus a small FFI wrapper for
  `SecCodeCopyGuestWithAttributes`.
- USB work: the existing `rusb` capture code, moved out of
  `clients/macos/src/usb_bridge.rs` largely unchanged.

## macOS 27 and Accessory Access

Apple pointed at the new `AccessoryAccess` framework as the eventual replacement
for both the entitlement and root. Public documentation describes
`AAUSBAccessoryManager.shared`, `AAUSBAccessoryMatchingCriteria`, and
`AAUSBAccessory.open(serviceQueue:completionHandler:)` returning an
`IOUSBHostDevice` — which is the full-transfer object Hard USB needs.

Two properties matter for our architecture:

- `AAUSBAccessoryManager` presents consent UI on the app's behalf and is
  therefore documented as usable **only from an app with a UI that appears in the
  Dock** — it cannot be driven from a daemon. Deck is such an app, so this is
  workable.
- `AAUSBAccessory.createXPCRepresentation()` / `init(XPCRepresentation:)` exist
  precisely to hand an opened accessory to a service process.

So the macOS 27 shape is an inversion, not a deletion, of this design: Deck opens
the accessory with user consent and passes the handle over XPC to a helper that
no longer needs to be root. Keeping the XPC boundary and the URB frame contract
identical now is what makes that migration cheap later. **Do not** design the
helper in a way that assumes it must always be the thing that opens the device.

## Open questions to resolve before implementation

Questions 3 and 5 below were closed on 2026-08-20 by direct SDK inspection on
macOS 26.6.1 (build 25G76). The rest are genuinely unresolved and none should be
written up as settled.

1. **Does `AAUSBAccessory.open()` actually pre-empt `AppleUserHIDDevice` on a
   HID-class Wacom?** Not documented publicly, and not answerable here:
   `AccessoryAccess.framework` is present in neither
   `$(xcrun --show-sdk-path)/System/Library/Frameworks` nor
   `/System/Library/Frameworks` on macOS 26.6.1, so the API cannot be compiled or
   probed on this machine at all. `IOUSBHost.framework` — the framework its
   `open()` returns into — *is* present. This is the single question that decides
   whether macOS 27 removes the helper or merely changes its shape, and it is
   worth a DTS follow-up on the existing case rather than a local experiment.
2. **Is `com.apple.developer.accessory-access.usb` self-service or restricted for
   this use?** Our own evidence says the `com.example.arcen.deck` App ID already carries
   it as a self-service "Claim USB Accessory" capability and that a profile
   containing it signed, notarized, stapled, and verified cleanly. Third-party
   summaries claim it is approval-gated. Our measured evidence is the stronger
   source; do not treat this as blocked without re-testing.
3. **~~Exact availability of `NSXPCListener.setConnectionCodeSigningRequirement`.~~
   Closed 2026-08-20.** Both halves of the pinning design exist and are macOS 13+,
   confirmed in `Foundation.framework/Headers/NSXPCConnection.h`:

   ```objc
   // line 161
   - (void)setConnectionCodeSigningRequirement:(NSString *)requirement
       API_AVAILABLE(macos(13.0)) API_UNAVAILABLE(ios, tvos, watchos);
   // line 118
   - (void)setCodeSigningRequirement:(NSString *)requirement
       API_AVAILABLE(macos(13.0)) API_UNAVAILABLE(ios, tvos, watchos);
   ```

   `FoundationErrors.h` additionally defines
   `NSXPCConnectionCodeSigningRequirementFailure = 4102`, which is the rejection
   the helper should expect to log. The listener-side API may therefore be the
   primary control. Keep the audit-token path as defense in depth anyway, since it
   is the only thing that also covers a future non-`NSXPCListener` shape.
4. **~~Whether `BundleProgram` can point at the existing fused binary with a
   subcommand.~~ Mechanism closed 2026-08-20; the policy choice stays open.**
   `man 5 launchd.plist` states that `BundleProgram` "maps to the first argument
   of execv(3) and is an app-bundle relative path to the executable for the job",
   and is "only supported for plists that are installed using SMAppService",
   while `ProgramArguments` "maps to the second argument of execvp(3) and
   specifies the argument vector". They therefore coexist: `BundleProgram` selects
   the executable and `ProgramArguments` supplies argv.

   So the fused shape **is** mechanically available — a plist could name
   `Contents/MacOS/arcen-deck` with `ProgramArguments` of
   `["arcen-deck", "usb-bridge-helper"]` and match the Linux convention exactly.
   This strengthens rather than weakens the need for a decision: §2 is not
   claiming fusion is impossible, it is arguing we should decline a capability we
   demonstrably have, because it would place the GUI, renderer, decoder, and
   transport inside a root process image. That is a Shared/Architecture and
   Release/Security judgement call, and it must be made explicitly rather than
   settled by what the mechanism happens to allow.
5. **~~Whether `SMAppService` exposes the status values this design depends on.~~
   Closed 2026-08-20.** `ServiceManagement.framework/Headers/SMAppService.h`
   defines `SMAppServiceStatus` with `NotRegistered`, `Enabled`,
   `RequiresApproval`, and `NotFound`, so the "route the user to Login Items
   instead of reporting Hard USB as broken" behavior is implementable as
   described. The same header set marks `SMLoginItem` as
   `__OSX_DEPRECATED(10.6, 13.0, "Please use SMAppService instead")`, confirming
   the direction of travel.
6. **Helper lifetime and idle behavior.** A `RunAtLoad` root daemon that lives for
   the whole session is more surface than one launched on demand. Decide between
   on-demand activation and an idle-exit timeout.

## A contradiction worth recording

Public `libusb` issues (notably libusb#1851) report `darwin_claim_interface`
failing with `kIOReturnExclusiveAccess` for HID-class devices on macOS 26, and
some summaries generalize that to "even root cannot bypass HID ownership on
macOS 26+".

That generalization does not match our own measurement. On 2026-08-13, on macOS
26.6.1, root whole-device capture of `056a:0317` succeeded and is recorded in
[`../operations/hard-usb-lab.md`](../operations/hard-usb-lab.md):

```text
device_capture=ok
interface=0 claim=ok
interface=1 claim=ok
interface=2 claim=ok
device_release=ok
```

Linux then enumerated the real device and bound `wacom.ko`. The difference is
that Arcen captures the **whole device** first (`USBDeviceReEnumerate` with the
capture bit) rather than claiming interfaces against a live HID driver, which is
the case the public issue describes.

Our measured evidence supersedes the public summary here. The risk to carry
forward is narrower: this behavior is a per-OS-version property, so it must be
re-measured on each macOS release rather than assumed, and the helper must fail
closed and restore ownership when capture is refused.

## Definition of done

- Ordinary notarized, non-root Deck completes a Hard USB session end to end.
- The helper independently denies a device Deck falsely claims is approved.
- An unsigned or wrongly-signed local process cannot open the Mach service.
- Helper crash, Deck crash, session loss, and logout each restore the Wacom to the
  macOS HID stack, verified by the non-root claim probe.
- Removing the app removes the daemon registration.
- Release validation covers the nested helper, and the signing step no longer uses
  `--deep`.
- The pending physical pen-report retest from
  [`../operations/hard-usb-lab.md`](../operations/hard-usb-lab.md) is closed
  first, so the helper is not being built on top of an unverified transport fix.

## Appendix: draft DTS follow-up for open question 1

Open question 1 cannot be answered locally — `AccessoryAccess.framework` is on
neither the macOS 26.6.1 SDK nor the running system. Quinn offered follow-up on
the existing case, so the question below is drafted ready to send. It is
deliberately narrow and answerable; it asks about driver arbitration, not about
whether we may have an entitlement.

Review with Release/Security before sending, since it describes product intent.

> Case-ID: 21584866
>
> Thank you — that resolves it. We have dropped the entitlement request and are
> building a Developer-ID privileged helper as you described, with XPC and
> `NSXPCListener.setConnectionCodeSigningRequirement` on the boundary.
>
> One follow-up about the macOS 27 direction, so we design the helper for a cheap
> migration rather than a rewrite.
>
> Our device is a HID-class USB pen tablet (Wacom Intuos5 touch L, `056a:0317`,
> three HID interfaces). We do not want to read its HID reports; we need to take
> the whole device away from the system HID stack, answer its descriptors and
> control transfers ourselves, forward its real control and interrupt traffic to a
> remote host so the vendor driver there binds the genuine device, and then return
> ownership to macOS on teardown. Today, root plus whole-device capture via
> `USBDeviceReEnumerate` achieves exactly this, and we have verified that
> ownership is restored afterwards.
>
> Under Accessory Access on macOS 27, does `AAUSBAccessory.open(serviceQueue:completionHandler:)`
> pre-empt the in-kernel HID driver for a HID-class device of this kind, giving
> the returned `IOUSBHostDevice` the same exclusive whole-device access we get
> from root capture today? Or is Accessory Access intended for devices that are
> not already claimed by an Apple driver, leaving the privileged-helper route as
> the supported path for HID-class accessories?
>
> Relatedly: we expect to do the transfer work in a helper process rather than in
> our UI app. Is `createXPCRepresentation()` / `init(XPCRepresentation:)` the
> intended way to hand the opened accessory to that process, and does the
> exclusive claim survive that transfer?
>
> The practical question is whether macOS 27 lets us drop the privileged helper
> for this device class, or whether we should plan to keep it.
