# Input-only native USB bridging for specialty devices

**Status:** physical macOS-to-Linux lab path implemented 2026-08-13; the
physical retest closed 2026-08-24 with three consecutive sessions from an
unprivileged Deck, so native Linux Wacom enumeration and repeatable
capture/release are both proven. Written 2026-07-29.
**Owners:** Shared/Architecture, macOS Client, Linux Host, Windows Host, with
Release/Security owning the new kernel/driver/signing trust boundary.

## The outcome we want

Arcen should support unusual USB **input** devices that cannot be represented
faithfully through its typed keyboard, mouse, trackpad, or pen contracts.

Examples include:

- Wacom, Xencelabs, and other pen tablets or pen displays whose native host
  driver/Wintab features matter;
- 3Dconnexion/SpaceMouse-style six-axis controllers;
- specialty keyboards, trackballs, mice, trackpads, jog/shuttle wheels, and
  grading panels;
- other reviewed controls whose normal function is interactive input.

This is **not** a general USB forwarding feature. Arcen must never become a
network path for flash drives, storage, hubs, cameras, microphones, printers,
smart cards, network adapters, serial adapters, firmware-update devices, or
arbitrary USB peripherals.

Ordinary keyboard, mouse, trackpad, and pen input continues to use Arcen's
smaller and safer typed input protocol. Native USB bridging is reserved for an
exact, host-approved specialty-device profile that needs host-side driver
semantics.

## The three input paths must remain distinct

In conversation these are referred to as **"Light"** (the implemented typed
path) and **"Hard"** (the native USB bridge). The raw-HID row is neither: it is
lab scaffolding, and §"Why the existing experimental raw-HID path is not the
bridge" explains why it cannot grow into the Hard path.

| Path | Shorthand | State | Device driver placement | Wire content | Intended network |
| --- | --- | --- | --- | --- | --- |
| **Typed input / Tablet support** | **Light** | Implemented | Client driver decodes the physical device; no vendor driver required on the host | Validated keyboard/mouse/pen semantics | WAN and LAN |
| **Experimental raw HID** | — | Quarantined, default-off | Client reads raw input reports; Linux creates UHID device | Descriptor + one-way input reports only | Lab evidence only |
| **Native input-device USB bridge** | **Hard** | Linux lab: physical enumeration and repeatable capture/release proven 2026-08-24; Windows has no importer | Deck exclusively owns the physical device; vendor driver binds on host | Bidirectional control, HID feature/output, interrupt, lifecycle, and completion semantics | LAN/KVM only |

Light is the default and stays the default: it works over WAN, needs no vendor
driver or kernel module on the host, keeps untrusted USB descriptors out of the
host's USB stack, and leaves the tablet usable by local macOS applications.
Hard exists for devices whose native driver semantics cannot be represented in a
typed contract, and it inverts every one of those properties.

The implemented Wacom **Tablet support** path is documented in
[`../architecture/pen-tablet-input.md`](../architecture/pen-tablet-input.md)
and validated by
[`../operations/pen-tablet-input.md`](../operations/pen-tablet-input.md).
It uses AppKit typed pen events, `PenEventMsg`, Linux `uinput`, and Windows
synthetic `PT_PEN`. It is the default because it:

- keeps untrusted USB descriptors out of the host's USB stack;
- tolerates normal WAN latency better;
- uses a small, bounded, vendor-neutral input contract;
- does not require a Wacom/vendor driver or new signed kernel driver on the
  host;
- leaves the tablet available to local macOS applications.

Native bridging inverts those properties. Deck must stop local use, the host
must see the device's real identity, synchronous USB/HID transactions cross the
network, and host kernel/driver parsers become part of the remote attack
surface.

## Implemented lab vertical slice (2026-08-13)

Arcen now has a default-off, installed macOS-to-Linux Hard USB lab path:

- safe shared `arcen-usb-bridge` policy/state/descriptor/URB core;
- capability-negotiated normalized URB frames in `arcen-protocol`;
- reliable delivery on the existing authenticated QUIC session;
- a fused, isolated Linux `usb-bridge-helper` owning `/dev/usb-vhci`;
- a separately packaged public SourceForge `usb-vhci` v1.15 DKMS module with
  independently authored RHEL 9 compatibility patches;
- a synthetic `Arcen USB Bridge Lab Tablet` whose USB/HID descriptors and
  reports are independently authored;
- real AppKit Wacom pen events driving that synthetic USB tablet.

Measured on pier-linux.example.internal:

```text
usb 9-1: Product: USB Bridge Lab Tablet
input: Arcen USB Bridge Lab Tablet .../0003:FFFF:A2CE.../input/input200
hid-generic ... input,hidraw1: USB HID v1.11 Device [Arcen USB Bridge Lab Tablet]
```

The authenticated `input-smoke --tablet-mode hard` sent 750 changing pen
states through Deck -> QUIC -> Pier -> helper, and teardown left zero helper
processes, HID devices, or virtual controllers.

The synthetic result above remains a deterministic regression mode. The
physical adapter now captures `056a:0317` through libusb and forwards its real
descriptor/control/interrupt traffic. pier-linux.example.internal enumerated the full-speed
device, bound `wacom.ko`, and created native Pen, Pad, and Finger inputs.
Ordinary idle interrupt timeouts initially triggered Linux resets; Deck now
keeps those URBs pending until a report or cancellation. Physical pen-report
validation of that final fix remains pending.

## Why the existing experimental raw-HID path is not the bridge

The default-off path under `clients/macos/src/hid/`, the binary HID frames in
`shared/protocol/src/wire.rs`, and Linux `/dev/uhid` admission currently carry:

- device add/remove;
- vendor/product ID;
- HID report descriptor;
- one-way input reports from Deck to Linux.

It does **not** carry:

- control requests and completions;
- HID `GET_REPORT` / `SET_REPORT`;
- feature reports;
- output/interrupt-OUT reports;
- configuration/interface/alternate-setting changes;
- endpoint halt/reset;
- suspend/resume or complete USB lifecycle/error state;
- host-to-device traffic;
- a Windows importer.

It also captures with non-exclusive `IOHIDDeviceOpen(...,
kIOHIDOptionsTypeNone)`, so the local macOS stack remains an owner.

That path is useful evidence for bounded queues, descriptor/report size limits,
vendor/interface filtering, and teardown. It must not be extended into the
production bridge. Once the real bridge has replacement coverage, remove the
experimental Cargo feature, runtime environment switch, legacy HID frames, and
Linux admission path.

## Scope boundary

### Always prefer typed semantic input

The following remain on `arcen-input` / `arcen-protocol` unless an exact
specialty profile proves that host-driver behavior is required:

- ordinary keyboards;
- ordinary relative and absolute mice;
- ordinary trackpads;
- Wacom/local pen operation through Tablet support;
- ordinary wheel, button, and keyboard mappings emitted by client-side device
  software.

A specific unusual keyboard or mouse may be bridge-eligible, but only as an
exact reviewed profile. There is no class-wide "all keyboards" or "all mice"
native-bridge rule.

### Native bridge v1 transfer envelope

The first bridge supports only what reviewed input devices need:

- control endpoint requests required for enumeration and approved device
  operation;
- interrupt IN;
- interrupt OUT;
- HID input, output, and feature reports;
- reset, clear-halt, suspend/resume, configuration, interface, and detach
  lifecycle required by the selected backend.

V1 excludes:

- isochronous transfers;
- general-purpose bulk data;
- USB hubs and downstream enumeration;
- storage or file transfer;
- firmware update/DFU;
- arbitrary vendor control requests not required by a reviewed device profile.

If a specialty input device requires bulk transfers or a wider vendor protocol,
that is a new reviewed profile and possibly a new product boundary, not an
automatic widening of v1.

### Permanently prohibited device/interface classes

The following classes reject the whole device regardless of an allowlist entry
or compatibility boolean:

| USB class | Prohibited use |
| --- | --- |
| `01` | Audio |
| `02`, `0a`, `e0` | Communications, CDC data, network/RNDIS/wireless |
| `06` | Imaging/PTP |
| `07` | Printer |
| `08` | Mass storage |
| `09` | Hub |
| `0b` | Smart card |
| `0e` | Video/camera |
| `dc` | Diagnostic |
| `fe` | Application-specific, including DFU/firmware update |

Vendor-specific class `ff` is denied unless the exact device profile lists the
interface and the implementation has a reviewed input/control reason for it.

## Threat model and honest limitations

The physical device, Deck, network peer, descriptors, reports, control
payloads, persisted configuration, and host kernel/device driver are separate
trust inputs.

The bridge must defend against:

- a compromised Deck trying to present an unapproved device;
- a malicious or modified USB device presenting deceptive descriptors;
- a composite device hiding storage/network/firmware interfaces beside a
  legitimate HID interface;
- BadUSB-style keystroke injection from a device that is technically valid HID;
- descriptor/report/control payloads targeting host USB/HID parser bugs;
- transfer floods, oversized lengths, unbounded in-flight requests, hotplug
  storms, stale completions, and generation reuse;
- a dropped connection leaving keys, buttons, contact, or exclusive ownership
  stuck;
- a local configuration mistake authorizing a wider device family than
  intended.

An allowlisted VID/PID is **not cryptographic device attestation**. USB
identifiers and descriptors can be spoofed. An allowlist reduces accidental and
known-class exposure; it cannot prove that peripheral firmware is benign. The
roadmap must not claim that an allowlisted device is malware-free.

Likewise, a valid approved keyboard report can still contain malicious
keystrokes. Report-rate and queue bounds limit floods and denial of service; they
cannot determine user intent. Native bridge enablement is an administrator
decision to trust the physical device, not an Arcen malware verdict.

## Host-authoritative Pier allowlist

Authorization belongs in the common strict Pier configuration:

- Linux: `/etc/arcen/pier.json`
- Windows: `%ProgramData%\Arcen\pier.json`

Deck requests a discovered device and provides bounded facts. The host
independently evaluates its own config and backend capability. A compromised
client cannot authorize itself.

The host derives the interface/endpoint set from the bounded descriptor
snapshot it parses itself; it does not trust a client-supplied "this is HID"
summary. This still cannot attest that the physical device matches bytes
supplied by a compromised Deck, but it prevents benign parser differences from
silently widening host policy.

The proposed future schema lives under `redirection.usb_input_bridge`:

```json
{
  "redirection": {
    "timezone": false,
    "usb_input_bridge": {
      "enabled": false,
      "devices": [
        {
          "name": "wacom-intuos5-touch-l",
          "enabled": true,
          "vendor_id": "056a",
          "product_id": "0317",
          "allow_all_hid_interfaces": false,
          "interfaces": [
            {
              "class": "03",
              "subclass": "00",
              "protocol": "00",
              "usage_page": "0d"
            }
          ]
        }
      ]
    }
  }
}
```

This is a proposed roadmap schema, not a currently accepted config field.
The example illustrates the field shape, not a complete Intuos5 interface
manifest. Before shipping it, physical descriptor capture must list every
required interface and prove that no omitted interface is necessary for native
host-driver operation.

### Field contract

- `usb_input_bridge.enabled`: host-wide bridge authority. Packaged configs
  remain off until the platform backends, signing, and release gates exist.
- `devices`: bounded list of exact device profiles.
- `name`: bounded stable profile name used in UI and lifecycle logs.
- `enabled`: whether this exact profile may be requested.
- `vendor_id`, `product_id`: exactly four lowercase hexadecimal digits. No
  wildcard and no "all devices from vendor `056a`" rule.
- `interfaces`: the complete expected interface set. Each entry uses exact
  class/subclass/protocol and, for HID, an optional usage page.
- `allow_all_hid_interfaces`: compatibility escape hatch for device revisions.

The existing protected ownership/ACL rules for `pier.json` apply. Deck has no
write path to this file, and malformed or unknown fields fail configuration
validation rather than being ignored.

### Shipped defaults

- Exact reviewed Wacom profiles ship with `enabled: true`.
- The first profile candidate is the physically observed Intuos5 Touch L,
  `056a:0317`; its full interface/transfer profile still requires bridge-mode
  physical capture and review.
- Additional Wacom models require exact VID/PID/interface evidence before they
  are added. "All Wacom" is never a wildcard.
- Non-Wacom example profiles ship with `enabled: false`.
- The global bridge remains unavailable until the relevant platform backend is
  installed and reports runtime availability.

### Strict interface behavior

With `allow_all_hid_interfaces: false`:

1. Device-level VID/PID must match.
2. Every exposed interface must match one expected entry.
3. Every expected required interface must exist.
4. Any unexpected interface rejects the **whole device**.
5. Any permanently prohibited class rejects the whole device.

With `allow_all_hid_interfaces: true`:

- additional HID-class interfaces on that exact VID/PID are accepted;
- explicitly listed vendor-specific input/control interfaces remain allowed;
- the setting does not authorize another VID/PID;
- the setting does not authorize hubs, storage, network, serial, audio, video,
  smart-card, imaging, diagnostic, or DFU interfaces;
- the wider decision is logged as a bounded high-risk policy fact, never with
  descriptors or reports.

This boolean is deliberately simple. If it becomes too broad in physical
testing, replace it with a more expressive versioned profile rather than
silently changing its meaning.

### Future hardening, not v1 requirements

Optional serial matching and descriptor SHA-256 pinning can detect accidental
replacement or descriptor drift, but they remain spoofable and are not
attestation. They should be evaluated after the simple VID/PID/interface
policy has real hardware evidence.

## Authorization and lifecycle

### Two-phase attach

1. Deck enumerates device/interface facts without claiming exclusive ownership.
2. Deck sends a bounded descriptor/configuration snapshot and offer for the
   selected saved connection and requested Native mode.
3. Pier validates authenticated session binding, generation, profile, the
   interface/endpoint set derived by its own parser, backend, network policy,
   and resource limits.
4. Pier returns an explicit accepted/denied result and selected backend.
5. Deck claims the device exclusively.
6. Deck acknowledges exclusive ownership.
7. Pier creates the virtual device and completes host enumeration.

The host must not create a kernel-facing device before policy acceptance, and
Deck must not seize the physical device before host acceptance.

After acceptance, both Deck and Pier enforce the same immutable endpoint map.
Deck rejects a host request outside the accepted attachment, and Pier rejects a
completion that does not match an outstanding request.

### Deck user experience

The existing **Native tablet mode** choice remains the connection-level request,
but a real implementation also needs a bounded specialty-device selector:

- list only local devices that match a known input profile;
- show the exact profile name, driver placement, and host policy result;
- state that the device becomes unavailable to local Mac applications while
  bridged;
- require an explicit device selection rather than forwarding every matching
  device;
- show requested versus active backend and a bounded denial reason;
- never display a generic "forward all USB" control.

The current `usb_auto_forward_non_hid` setting and the UI text that mentions
flash drives do not match this roadmap. A future implementation must migrate or
retire that setting instead of reusing it as authorization. The replacement UI
is **Specialty input devices**, not non-HID USB forwarding.

### Teardown order

On disconnect, terminal session end, reconnect deadline, policy change,
descriptor/interface change, timeout, client/backend failure, or explicit
user disable:

1. stop accepting new transfers;
2. cancel/complete bounded in-flight transfers;
3. release virtual keys/buttons/contact and destroy the host virtual device;
4. acknowledge host teardown when possible;
5. release exclusive ownership on Deck;
6. let the local OS/vendor driver reclaim the physical device.

A stale generation cannot complete a transfer or resurrect a destroyed device.

### Privacy and audit

Allowed lifecycle telemetry:

- profile name;
- accepted/denied and bounded reason class;
- backend kind;
- attachment generation;
- device/interface counts;
- transfer count, queue depth, latency, timeout, reset, and drop counters.

Never log or persist:

- raw descriptors;
- serial numbers;
- report/control payloads;
- key codes, button state, coordinates, pressure, or other input content;
- raw USB paths or device-instance identifiers.

Support bundles use the same exclusion.

## Shared transport: QUIC

The bridge must use the product-neutral `arcen-transport` contract. It must not
open its own WebSocket, TCP socket, UDP socket, Quinn connection, or custom
encrypted channel.

See [`../architecture/transport.md`](../architecture/transport.md) and
[ADR 0002](../adr/0002-transport-evolution.md). The proposed bridge adds its
own capability (for example `input:usb-bridge-v1`) but continues to require the
shared `delivery:reliable-stream-v1` capability.

Current direct QUIC uses these semantic classes:

| Bridge traffic | `MessageClass` | `ReliabilityClass` | Delivery |
| --- | --- | --- | --- |
| offer/result, attach/detach, descriptors, configuration, control, feature/output, reset, cancellation, completion, errors | `Control` | `Control` | `ReliableStream` |
| interrupt input reports/completions | `Input` | `InputLowLatency` | `ReliableStream` |

This means:

- the current `Quic` profile carries the bridge on its persistent reliable
  stream;
- certificate, peer identity, session binding, capability intersection,
  bounded queues, reconnect events, and profile downgrade/refusal follow the
  shared transport rules;
- a policy requiring QUIC or the bridge capability fails closed;
- USB bridging does not create a second transport.

The bridge protocol lives above `arcen-transport` and below product adapters,
the same way `arcen-protocol` remains independent of a concrete QUIC stream
layout.

### No bare UDP

The public
[`ESP32-UDP-Mouse-Relay`](https://github.com/victorpaglione11/ESP32-UDP-Mouse-Relay)
is useful as a small latency experiment, not as a production security model.
Useful lesson:

- accumulate and send only changed/latest relative motion rather than queueing
  every superseded sample.

Do not copy:

- unauthenticated/unencrypted UDP;
- hardcoded peer address as authorization;
- size-only packet admission;
- no version, type, sequence, timestamp, generation, or session binding;
- silent loss/reorder;
- no keepalive, disconnect reset, or release-all;
- a blocking sender as backpressure handling.

Typed key/button/contact edges and USB control/completion traffic are
authority-bearing facts. They remain reliable and ordered.

### QUIC datagrams are not v1 USB delivery

The accepted transport ADR currently permits `EncryptedDatagram` only for
`MediaLowLatency`; `InputLowLatency` is reliable-stream-only. Native USB v1
does not change that.

A later reviewed profile may consider QUIC datagrams for a narrowly identified
full-state interrupt report that is:

- explicitly coalescable;
- repaired by a later state snapshot;
- separated from key/button/contact/device edges;
- protected by sequence/lateness checks and release-all on liveness loss;
- supported by measured latency improvement.

That requires a new Shared/Architecture decision and transport tests. It is not
an implementation shortcut for this roadmap.

DSCP/EF marking may be evaluated as a managed-LAN optimization, but it is not a
security, delivery, or WAN guarantee.

## Future bridge protocol semantics

Final message names and wire numbers require a separate protocol review. The
capability-negotiated protocol needs at least:

- device offer and profile decision;
- attach generation and exclusive-ownership acknowledgment;
- bounded descriptor/configuration snapshot;
- control request and completion;
- HID `GET_REPORT` / `SET_REPORT`;
- feature and output report;
- interrupt IN completion and interrupt OUT request;
- set configuration/interface/alternate setting;
- reset, clear-halt, suspend/resume, detach, and bounded device error.

Every transfer carries or is bound to:

- session and attachment generation;
- bounded transfer ID;
- endpoint, direction, and transfer kind;
- sequence;
- declared payload length;
- bounded timeout/deadline;
- completion status and actual length;
- cancellation state.

Required invariants:

- additive capability negotiation; old peers never activate the feature;
- metadata and policy validation before payload allocation;
- fixed bounds for descriptors, payloads, devices, interfaces, endpoints,
  in-flight transfers, queue bytes/messages, and transfer rate;
- no completion for a stale generation;
- no duplicate transfer ID;
- no silent truncation;
- no unknown status converted to success;
- no raw-HID frame reuse;
- no coupling to `PenEventMsg`.

## Platform feasibility

### macOS Deck: physical-device exporter

This is the highest-risk prerequisite.

The current IOHID capture opens devices non-exclusively and reads input
reports. A native bridge needs:

- exclusive ownership away from macOS and the local Wacom/vendor driver;
- raw control and interrupt pipe access;
- deterministic restoration after disconnect/crash;
- exact VID/PID matching aligned with host policy.

Implemented lab direction:

- public rusb/libusb whole-device capture under root;
- exact Intuos5 touch L identity/revision/interface policy;
- concurrent bounded real control/interrupt URBs;
- cancellation tombstones and deterministic interface/driver restoration.

Production authorization no longer depends on an Apple entitlement grant. Apple
Developer Technical Support confirmed on 2026-08-20 (Case-ID 21584866) that
`com.apple.vm.device-access` is unavailable and unnecessary for Developer ID
distribution, because every operation it gates also admits a privileged
process. The remaining routes are a signed/notarized privileged helper reached
over IPC (required for supported releases, designed in
[`./macos-privileged-usb-helper.md`](./macos-privileged-usb-helper.md)),
Accessory Access on macOS 27 and later, or a separately identified DriverKit
extension. Input Monitoring TCC is not a replacement for USB authorization.

Public references:

- [USBDriverKit](https://developer.apple.com/documentation/usbdriverkit)
- [System Extension entitlement request](https://developer.apple.com/contact/request/system-extension/)
- [Implementing drivers as system extensions](https://developer.apple.com/documentation/systemextensions/implementing-drivers-system-extensions-and-kexts)
- [`kIOHIDOptionsTypeSeizeDevice`](https://developer.apple.com/documentation/kernel/1644502-anonymous/kiohidoptionstypeseizedevice)

The exclusive-claim model is proven: capture removes the tablet from macOS,
and disconnect restores Apple/Wacom ownership. The remaining lab gate is
physical report delivery after the pending-interrupt correction.

### Linux Pier: virtual-device importer

Keep today's `uinput` Tablet support unchanged.

Evaluate two backends:

1. **HID transport fast path:** UHID for a proven HID-only profile. It can
   provide input/output and HID report transactions with a narrower attack
   surface, but cannot emulate arbitrary USB control transfers.
2. **Full USB path:** an in-box `vhci-hcd`/USB-IP-class importer when the vendor
   driver needs real USB enumeration or non-HID control requests.

Arcen does not expose USB/IP's unauthenticated network service. The kernel
backend sits behind Arcen's authenticated transport and a bounded importer
helper.

The helper should:

- run outside the main Pier process;
- use namespaces/seccomp and drop capabilities after opening required devices;
- receive a typed bounded IPC protocol;
- validate descriptors/endpoints/transfers before kernel submission;
- fail closed and destroy the virtual device if IPC or policy becomes
  inconsistent.

Do not use Linux raw-gadget as a production backend; its documented purpose and
attack surface fit testing/fuzzing, not this product boundary.

Public references:

- [Linux USB/IP protocol](https://www.kernel.org/doc/html/latest/usb/usbip_protocol.html)
- [Linux UHID](https://docs.kernel.org/hid/uhid.html)
- [Linux raw gadget](https://docs.kernel.org/usb/raw-gadget.html)

### Answered 2026-08-12: Wacom requires the full USB path

The critical empirical question above — does the native driver work through
HID-level virtualization, or does it need a real USB device? — was measured on
pier-linux.example.internal (RHEL 9, `5.14.0-503.14.1.el9_5`). For Wacom the answer is
unambiguous: **backend 1 (UHID) cannot work at all**, and backend 2 is not an
optimization but the only option.

A `/dev/uhid` device created with Wacom's vendor ID binds **no driver**:

| uhid device | bound driver |
| --- | --- |
| vendor `0x1234` (no in-tree claimant) | `hid-generic` |
| vendor `0x056A`, `wacom.ko` loaded | **none** |
| vendor `0x056A`, `wacom.ko` not loaded | `hid-generic` (hidraw only) |

The control binds, so `/dev/uhid` itself is healthy and the failure is specific
to Wacom. `hid-generic` declines because Wacom is in the kernel's
`hid_have_special_driver` table, and `wacom.ko` does not take it either: an
explicit `bind` returns `ENODEV` with `wacom` and `hid` dynamic debug enabled
and no kernel output at all, so `wacom_probe` is never entered. `wacom.ko` is a
USB driver and a uhid device is not a USB device; no amount of matching work
changes that.

Measurement trap, since it cost a session: a `new_id` dynid persists until the
module is unloaded (`remove_id` is `Permission denied` under lockdown), and
while registered it makes every later run *appear* to reach probe and fail
`-EINVAL`, which reads as a probe bug and is not one. Clear it with
`modprobe -r wacom && modprobe wacom` before believing any result.

This also confirms, empirically, this document's existing position that the
experimental raw-HID path is not the bridge and must not be extended into one.

### Correction: `vhci-hcd` is not in-box on the target platform

The "in-box `vhci-hcd`/USB-IP-class importer" above is **not available on
RHEL 9**. The only `*vhci*` module in `5.14.0-503` is Bluetooth's unrelated
`hci_vhci`; there is no `vhci-hcd`, no `usbip-core`, and `usbip-utils` is
neither installed nor offered by the configured repos.

So the Hard path requires shipping an **out-of-tree kernel module**, with the
consequences that follow: DKMS packaging, a rebuild on every kernel update,
Secure Boot signing, and a support burden on hosts whose kernel the customer
controls. That is a Release/Security decision and a distribution question, not
an implementation detail — and it should be taken before any code is written.

The reference product resolves it exactly this way, which is useful
corroboration that no in-box route exists: incumbent commercial remote-desktop
agents on Linux also ship an out-of-tree virtual USB host-controller kernel
module and its ioctl interface, rather than finding an in-box mechanism that
avoids one. Their device support is likewise gated on an explicit
vendor/product allow-list, and their generic-HID and mouse/keyboard paths are
separate mechanisms from USB passthrough — three distinct paths, matching this
document's three-path model rather than contradicting it.

That conclusion is drawn from publicly observable packaging and runtime
behaviour. No third-party source was copied, ported, or derived; see
[`../../legal/ORIGINS.md`](../../legal/ORIGINS.md).

### Consequence for the client entitlement question — resolved 2026-08-20

`com.apple.vm.device-access` had been recorded in
[`../../clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`](../../clients/macos/APPLE_ENTITLEMENT_REQUESTS.md)
as "not needed", on the strength of a measurement that `IOHIDManager` captures a
Wacom with no entitlement at all (true, and reproducible via the hardware test
in `clients/macos/tests/tablet_capture_hardware.rs`).

The reasoning behind reopening it still holds: HID capture yields input reports,
while a virtual USB device must answer descriptors, control transfers and URBs,
which input reports are not. So the client really does have to claim at the USB
layer, or the host must synthesise the device from a per-model descriptor set.

What changed is the cost of the first option. Apple Developer Technical Support
answered the request on 2026-08-20 (Case-ID 21584866): the entitlement is
reserved for Mac App Store hypervisor apps supporting systems before macOS 27,
it exists only because those apps cannot escalate privileges, and every
operation it gates also admits a privileged process. A Developer-ID-signed Deck
therefore neither needs nor can obtain it.

So the original "not needed" verdict is restored, but for a completely
different reason than the one first recorded. USB-layer claiming stays on the
table and is no longer blocked on Apple; it is blocked on Arcen shipping a
small root helper with a reviewed IPC boundary, replaced by Accessory Access on
macOS 27 and later. Helper packaging, signing, and the privilege boundary are
Release/Security's call.

### Windows Pier: virtual-device importer

Keep today's synthetic `PT_PEN` Tablet support unchanged.

Evaluate:

1. **VHF** for a proven HID-only profile. VHF supports bidirectional HID
   input/output/feature transactions but does not provide a real USB PDO.
2. **UDE/UdeCx** when the vendor stack requires genuine USB enumeration,
   descriptors, endpoints, or arbitrary control requests.

Both require a minimal KMDF kernel driver. Network parsing, session policy,
allowlist evaluation, and complex lifecycle remain in a user-mode Arcen
service/helper behind bounded IOCTLs.

Release gates include:

- EV organization code-signing certificate and protected key;
- Microsoft Partner Center signing;
- attestation signing for the supported direct-distribution matrix, or
  WHQL/HLK where Windows Update/Server support requires it;
- HVCI/Memory Integrity compatibility;
- driver package upgrade, rollback, uninstall, and crash recovery;
- installer opt-in and explicit reboot behavior.

Public references:

- [Virtual HID Framework](https://learn.microsoft.com/en-us/windows-hardware/drivers/hid/virtual-hid-framework--vhf-)
- [Developing emulated USB host/device drivers](https://learn.microsoft.com/en-us/windows-hardware/drivers/usbcon/developing-windows-drivers-for-emulated-usb-host-controllers-and-devices)
- [UDE architecture](https://learn.microsoft.com/en-us/windows-hardware/drivers/usbcon/usb-emulated-device--ude--architecture)
- [Driver signing requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs)
- [Attestation signing](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation)

Critical empirical question: will each vendor stack bind to a VHF virtual HID,
or does it require a UDE-created USB PDO?

## Low-latency policy

Native bridge is **LAN/KVM only**. Tablet support remains the recommended WAN
mode.

V1 latency rules:

- all transport delivery remains reliable;
- bridge control traffic has its own bounded scheduling lane so media work
  cannot starve enumeration/completions;
- interrupt input has a small bounded queue;
- only superseded motion/full-state reports may coalesce before transport;
- key, button, contact, tool, device, configuration, control, output, feature,
  reset, and release edges never drop;
- queue-full is an explicit attachment failure or reset, not silent loss;
- record queue delay, transfer RTT, timeout, reset, and loss counters without
  payload content.

Do not invent a latency threshold before measurement. Acceptance should measure:

- physical event to Deck capture;
- capture to transport enqueue;
- transport RTT and queueing;
- host importer completion;
- host driver/application presentation.

Test clean Ethernet, managed Wi-Fi, controlled added RTT, loss, reorder,
disconnect, and path migration. Enumeration/control timeouts will define the
real supported envelope.

## Phased roadmap

### Phase 0 — policy and threat model

- finalize this scope with Shared/Architecture and Release/Security;
- define the strict common Pier config schema and built-in Wacom profiles;
- define hard prohibited classes and bounded transfer policy;
- model profile matching as pure code with negative tests;
- define additive protocol capabilities and transport envelopes;
- obtain legal/source approval for every third-party dependency before use.

### Phase 1 — macOS exclusive-capture proof

- use the Intuos5 Touch L `056a:0317`;
- prove claim, descriptor/control/interrupt access, disconnect, crash recovery,
  and local-driver restoration;
- decide whether DriverKit is mandatory;
- submit Apple entitlement request early if required.

This phase is first because macOS exclusive ownership is the largest schedule
risk.

### Phase 2 — Linux backend comparison

- test UHID HID-only virtualization with the real Wacom driver;
- test a `vhci-hcd`/USB-IP-class importer for full enumeration/control;
- prove pressure/tilt/buttons and native driver behavior in a creative app;
- run the importer in an isolated helper;
- select backend per device profile, preferring the narrower sufficient path.

### Phase 3 — Windows backend comparison

- build test-signed VHF and UDE prototypes;
- prove Wacom driver/Wintab binding and full pen behavior;
- select the narrower sufficient backend per profile;
- document the signing/installer path before production work.

### Phase 4 — shared protocol and transport integration

- add capability-negotiated bridge DTOs/contracts;
- route all envelopes through `arcen-transport`;
- validate identical semantics under the current and future QUIC profiles;
- add generation, timeout, reconnect, and teardown tests;
- do not activate datagram delivery.

### Phase 5 — security hardening

- fuzz profile, descriptor, endpoint, transfer, and completion parsers;
- test malicious composites and all prohibited classes;
- add rate/size/in-flight/device/interface bounds;
- add helper/driver IPC validation and crash containment;
- verify logs/support bundles contain no input or device-private payloads;
- run Release/Security threat review.

### Phase 6 — signing, packaging, and physical release evidence

- Apple entitlement, system extension, Developer ID, notarization, staple;
- Windows signed driver package, HVCI, install/upgrade/rollback/uninstall;
- Linux helper permissions and kernel-module posture;
- operator config documentation and safe rollback;
- hardware soak and latency acceptance on Linux and Windows.

### Phase 7 — device-family expansion

- add exact reviewed profiles for additional Wacom models;
- add non-Wacom tablet, SpaceMouse/3D control, and specialty keyboard/control
  profiles disabled by default;
- never add a vendor-wide wildcard;
- require physical validation for every enabled-by-default profile.

### Phase 8 — remove experimental raw HID

After the real bridge replaces its useful coverage:

- remove `experimental-raw-hid` Cargo features;
- remove `ARCEN_EXPERIMENTAL_RAW_HID`;
- remove old HID binary frame types/codec;
- remove the macOS raw-HID session and Linux UHID admission path;
- update architecture, operations, provenance, and compatibility evidence.

## Mandatory validation matrix

### Positive hardware

- Wacom profile enabled by default: host driver binds, pressure/tilt/eraser,
  buttons, proximity, output/feature traffic, reconnect, and local restoration.
- At least one additional tablet profile.
- At least one SpaceMouse/3D input profile.
- At least one specialty keyboard/control profile where typed input is
  insufficient.
- Ordinary keyboard/mouse/trackpad remain on typed input and are not seized.

### Policy and malicious-device negatives

- unlisted VID/PID rejected;
- vendor-wide spoof rejected;
- unexpected interface rejects whole device in strict mode;
- unexpected HID interface accepted only for the exact profile with
  `allow_all_hid_interfaces: true`;
- any permanently prohibited interface rejects the whole device even with the
  boolean enabled;
- HID+storage, HID+network, HID+serial, HID+audio/video, hub, and DFU composites
  rejected;
- missing required interface rejected;
- invalid class/subclass/protocol/usage value rejected at config load;
- unknown config field rejected by the common strict schema;
- oversized descriptor/report/control payload rejected before kernel/driver;
- invalid endpoint, direction, transfer kind, completion length/status, timeout,
  duplicate ID, stale generation, flood, and hotplug storm rejected;
- compromised client cannot override host policy.

### Lifecycle and network

- exclusive claim refused/rolled back cleanly;
- local driver returns after normal detach, network loss, Deck crash, Pier
  crash, helper/driver crash, service restart, and host reboot;
- no stuck key/button/contact or orphan virtual device;
- current and future QUIC profile tests produce the same protocol decisions;
- controlled RTT/loss/reorder/path-change tests;
- no raw input or device-private payload in logs/support bundles.

### Release

- Apple entitlement/profile/signing/notarization/system-extension approval;
- Windows production driver signing and HVCI validation;
- Linux least-privilege helper installation;
- installer upgrade/rollback/uninstall;
- SBOM, notices, provenance, and Release/Security approval;
- exact physical hardware evidence before any supported-device claim.

## Research precedents and lessons

Arcen is not copying these implementations. They inform architecture and
negative requirements:

- [usbredir filtering](https://gitlab.freedesktop.org/spice/usbredir/-/raw/main/usbredirparser/usbredirfilter.h):
  device plus per-interface matching, every interface must pass, unmatched
  defaults to deny.
- [USBGuard rule language](https://github.com/USBGuard/usbguard/blob/master/doc/man/usbguard-rules.conf.5.adoc):
  VID/PID, interface triplets, serial/hash concepts, and fail-closed policy.
- [USB-IF class codes](https://www.usb.org/defined-class-codes) and
  [HID 1.11](https://www.usb.org/sites/default/files/hid1_11.pdf):
  authoritative class/interface vocabulary.
- [Linux USB/IP protocol](https://www.kernel.org/doc/html/latest/usb/usbip_protocol.html):
  complete remote USB request/completion model, used only as a backend/protocol
  reference behind Arcen authentication.
- [Linux UHID](https://docs.kernel.org/hid/uhid.html):
  narrower HID transport option.
- [RFC 9221](https://www.rfc-editor.org/rfc/rfc9221.html):
  QUIC datagrams are appropriate only when loss is explicitly tolerable.

## Decisions this roadmap fixes

1. Native bridging is input-only, not generic USB forwarding.
2. Typed input stays the default and WAN path.
3. Host Pier config is authoritative.
4. Wacom exact profiles are enabled by default; other templates are disabled.
5. Strict interface matching is default.
6. `allow_all_hid_interfaces` is a per-profile compatibility boolean, not a
   global wildcard and not permission for prohibited classes.
7. Bridge v1 uses the shared QUIC transport and remains reliable-stream only.
8. macOS exclusive capture and Windows driver signing are hard prerequisites,
   not follow-up polish.
9. Experimental raw HID is removed after replacement, not retained as a second
   production bridge.

## Open questions for implementation spikes

- Can every target Wacom family be claimed and restored reliably through
  DriverKit on supported macOS versions?
- Does each host vendor stack bind to HID transport virtualization, or require a
  real USB device/PDO?
- Which exact vendor control requests are necessary per profile?
- What measured RTT/loss envelope permits stable enumeration and drawing?
- How many simultaneous specialty devices are required in the first release?
- Does the simple compatibility boolean remain acceptable after real descriptor
  drift and malicious-composite testing?
