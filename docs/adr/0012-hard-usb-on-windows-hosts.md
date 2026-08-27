# ADR 0012: Hard USB on Windows Hosts

**Status:** Proposed 2026-08-25. Not yet decided.

> Recorded so the mechanism question is answered once and the cost is visible
> before anyone starts. Needs Shared/Architecture sign-off for the driver
> boundary and Release/Security sign-off for the signing and distribution
> consequences. The physical-pen gate cleared on 2026-08-25; the remaining
> conditions are in [Sequencing](#sequencing).

## Context

Arcen offers three tablet modes. On Windows only two of them work, and the
asymmetry is deliberate rather than a defect:

| Mode | Windows today | Mechanism |
| --- | --- | --- |
| Tablet support (locally terminated) | **Works** | `CreateSyntheticPointerDevice` / `InjectSyntheticPointerInput`, `hosts/windows/src/input.rs` |
| Mouse compatibility only | Works | `SendInput` |
| Native tablet (USB bridged) | **Not implemented** | — |

`hosts/windows/src/session.rs` advertises
`wacom_usb_bridge: InputCapabilityAvailability::Unavailable` unconditionally, so
a Deck that asks for Native tablet against a Windows host is negotiated down and
told why. That is honest behaviour, not a bug, and the Deck's Input tab says so.

Linux gets Hard USB from `usb-vhci`, a virtual USB *host controller* kernel
module. The bridged device enumerates on a virtual bus and the vendor driver
(`wacom.ko`) binds to it exactly as it would to physical hardware. Windows has
no equivalent in the box, and that single missing piece is the whole gap.

### What prior art establishes

These observations come from publicly observable packaging and driver-class
behaviour of incumbent commercial remote-desktop products. **No code, binary, or
payload from any third party is used, and none is proposed here** — only the
mechanism and its requirements. See
[`legal/ORIGINS.md`](../../legal/ORIGINS.md).

Three facts are worth having:

1. **Bridged Wacom on Windows is a solved problem elsewhere**, with the
   same low-latency caveat as Linux — it is not blocked by anything intrinsic
   to Windows.
2. **The two tablet modes are served by two entirely different drivers.**
   Locally terminated termination uses a KMDF *virtual HID* driver on the
   standard Windows `HIDClass`; bridging ships as a wholly separate USB driver.
   Nothing in that design tries to serve both modes from one mechanism, which
   corroborates Arcen's own split between the typed path and the USB bridge.
3. **The usual local-termination approach is one Arcen does not need.** It
   relies on a signed kernel HID driver; `InjectSyntheticPointerInput` (Windows
   10 1809, October 2018) delivers real `PT_PEN` pressure, tilt and rotation
   from user mode, and that is what `hosts/windows/src/input.rs` already uses.
   Arcen's Windows Light path therefore requires **no driver, no EV certificate
   and no attestation**, and a Windows bridge must be built without disturbing
   it.

### The two modes are permanent, and they are not competing

Stated by the owner on 2026-08-25. This is product intent, not an implementation
detail, and it is what the mechanism below has to serve:

- **Tablet support** is the mode for Wi-Fi, 5G, and working away from the
  office. It is *good enough for most remote work*, and it is good enough
  precisely because the pen is interpreted on the Mac — latency costs the
  operator responsiveness, not correctness.
- **Native tablet** is for a LAN: the office is close, the link is fast, and the
  goal is to get as near to a direct USB-to-USB connection as a network allows.

Neither mode is a stepping stone to the other and neither is expected to
subsume the other. A Windows bridge exists to give LAN operators the second
mode; it is not a route to replacing the first.

## Proposed decision

### 1. A KMDF client driver on `UdeCx`, emulating a USB host controller

Windows USB Device Emulation (UDE) is the supported mechanism: a KMDF driver
using the `UdeCx` class extension presents a virtual USB host controller and
attaches emulated devices to it, and because the device appears behind a host
controller, the ordinary Windows USB and HID class drivers bind to it as though
it were physical. That is the precise analogue of `usb-vhci`, which is what lets
the same `arcen-protocol` URB contract serve both hosts unchanged.

The driver is a transport endpoint only. It must not parse tablet semantics,
must not know what a Wacom is, and must re-derive nothing that
`arcen-usb-bridge` already decides — the policy profile stays in safe shared
Rust on both ends, as it does today.

**Owner: Shared/Architecture.**

### 2. Scope limited to control and interrupt transfers

Hard USB v1 carries control and interrupt only. Isochronous is explicitly out:
the reference supports it on Windows for webcams, but it is a materially harder
contract (bandwidth reservation, no retries, deadline delivery) and no Arcen
requirement asks for it. A tablet needs neither.

Bulk is likewise out of v1. `usb_auto_forward_non_hid` remains the answer for
mass storage, and remains separate from tablet mode.

**Owner: Shared/Architecture.**

### 3. Signing, and why the cost is already owed

A KMDF driver must be signed. Attestation signing (EV certificate + Partner
Center) covers Windows 10/11 desktop; Windows Server needs WHQL/HLK. This is
the same pipeline `arcen-microphone` already needs — it is in the Windows
package manifest (`packaging/windows/verify-package-manifest.ps1`) as an
optional, off-by-default staged driver, and its remaining evidence is exactly
"protected EV/Partner Center and WHCP/HLK signing".

So Windows Hard USB does **not** open a new front. It is a second consumer of a
pipeline that must be built once regardless. That materially changes its cost,
and is the main reason to record this decision now rather than rediscover it.

Note that ADR 0008 is superseded for shipping, so IddCx is no longer a co-tenant
of that pipeline; `arcen-microphone` is.

**Owner: Release/Security.**

## Sequencing

**The gate on this ADR has cleared.** It previously said no work should start
until the physical Linux pen retest passed. It passed on 2026-08-25: a live GUI
session against pier-linux.example.internal bridged the real `056a:0317` and the operator confirmed
the pen drives the pointer, with the host log showing the typed device released
and one tablet on the seat. See
[`../operations/hard-usb-lab.md`](../operations/hard-usb-lab.md).

That removes the reason to defer the *decision*. It does not by itself justify
starting the *work*, for two remaining reasons:

- Several Hard USB capabilities are still unconfirmed on hardware — pressure and
  tilt inside a real application, a long idle followed by resume, and reconnect
  with a transfer pending. Building a Windows implementation on a design whose
  behaviour is only partly established would put any remaining defect behind a
  kernel driver, where it is far more expensive to correct.
- The signing pipeline is Release/Security work that gates `arcen-microphone`
  independently. It should be built once, on its own schedule, not as a side
  effect of a tablet feature.

## Alternatives rejected

| Alternative | Why rejected |
| --- | --- |
| Extend the existing typed path to cover Native tablet | Different guarantee entirely. The point of Native is that the *host's own vendor driver* owns the device, giving tablet buttons and finger touch. A synthetic pointer cannot provide that however it is extended. |
| A virtual HID driver, as the reference uses for local termination | Solves the mode Arcen already solves without a driver, and does not solve bridging. It would trade a working user-mode path for a signed kernel one and gain nothing. |
| USB/IP client on Windows | Reverses the direction: it consumes a remote device over TCP, whereas Arcen already carries URBs over its own authenticated QUIC session. Adding a second transport and trust boundary for the same bytes is strictly worse. |
| A user-mode driver (UMDF) | UDE client drivers are KMDF. There is no supported user-mode route to presenting a virtual USB host controller. |
| Ship a third-party virtual USB bus driver | Provenance and servicing. Arcen would inherit an unaudited kernel component into a signed package, which `legal/ORIGINS.md` does not permit and Release/Security would have to own forever. |
| Keep Windows on Light only, permanently | Defensible today and is the current state, but forecloses parity for LAN and KVM operators who specifically want the vendor driver's own behaviour. Recording the route costs nothing and keeps the option open. |

## Consequences

- Windows Hard USB stays unavailable and is negotiated down with an accurate
  reason, which is already the behaviour.
- The Deck's Input tab already tells the truth ("Linux hosts do, Windows hosts
  do not yet") and needs no change until this ships.
- If it is built, `arcen-protocol`'s URB contract and `arcen-usb-bridge`'s policy
  core are reused unchanged; the new code is a driver plus the host-side
  attach/detach lifecycle, not a second protocol.
- The `usb-hard-lab` feature gate and its exclusion from release artifacts
  continue to apply on both platforms until the boundary is approved.

## Sign-off

| Role | Decision | Who | Date |
| --- | --- | --- | --- |
| Shared/Architecture (items 1 and 2) | Not yet sought | — | — |
| Release/Security (item 3) | Not yet sought | — | — |
