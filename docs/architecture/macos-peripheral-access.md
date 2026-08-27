# macOS peripheral access: which route, and why

**Status: architecture note.** Records how the macOS Deck reaches local
peripherals today, why the two routes differ, and what a webcam would need.
Written so the next person does not reach for a privileged helper when ordinary
consent would do, or expect consent to grant something only root can.

## Two routes, not one

macOS has two entirely separate mechanisms, and the mistake worth avoiding is
treating them as one.

**Consent (TCC).** The user is asked once, per app, per category — microphone,
camera, screen recording, input monitoring. The app declares a purpose string in
`Info.plist`, the system prompts, and the answer is remembered. The app stays
unprivileged. No entitlement makes the prompt go away, and no entitlement
substitutes for the answer.

**Device capture.** Taking a USB device away from the driver that owns it, so
your process speaks to the hardware directly. TCC has nothing to say about this.
Apple's SDK header states the operation requires either
`com.apple.vm.device-access` plus `IOServiceAuthorize`, **or root**.

Arcen uses the first route where it can and the second only where it must.

| Peripheral | Route | Status |
| --- | --- | --- |
| Microphone | Consent | **Works.** `AVAudioEngine`, `NSMicrophoneUsageDescription`, opt-in per launch. |
| Graphics tablet, as pen input | Neither — the window already receives it | **Works.** AppKit `tabletPoint` events carry pressure and tilt. |
| Graphics tablet, as a real USB device | Device capture | Experimental, `usb-hard-lab`, not in releases. |
| Webcam | Undecided | Does not exist. See below. |

## Why the microphone needed no helper

It is worth being explicit, because the symmetry with USB is tempting and wrong.

The microphone is a consent problem. `clients/macos/src/microphone.rs` calls
`AVAudioEngine`, requests permission, and captures. The Deck stays unprivileged
and always has. Adding a root helper for it would buy nothing and would widen
the attack surface for a capability the system already grants safely.

The tablet is a device-capture problem, and only in one of its two modes.
Arcen's default path never touches the device: macOS delivers pen events to the
window, and `shared/input` carries pressure and tilt to the host. That path
needs no permission at all. Only *Hard USB* — where the host is meant to see the
real device, bind its own vendor driver, and read its raw descriptors — needs
the device taken away from Apple's HID stack.

## What Apple confirmed, and what follows

Measured against a real tablet with the additional entitlement granted, the
device opened but every interface claim failed with `exclusive access`. The
entitlement moves the device-level boundary; it does not detach Apple's HID
ownership. It is meant for the newer Accessory Access framework, which is not
present in the SDK or runtime on the machines this was tested on.

**Apple's 2026-08-20 answer confirmed the root branch is the intended one for
Developer ID distribution.** Full detail, including the measured interface-claim
output, is in
[`clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`](../../clients/macos/APPLE_ENTITLEMENT_REQUESTS.md).

That answer is the whole reason the helper exists. If device capture needs root,
the question becomes *what runs as root* — and the answer must not be the client.

## The helper pattern

[ADR 0011](../adr/0011-macos-privileged-usb-helper.md) settled it: a separate,
minimal binary, roughly 1,500 lines, that links none of the client's UI,
transport, or media code.

The reasoning is worth repeating because it generalises. On Linux the helper is
spawned by an already-privileged service, so folding it into the main binary
costs nothing. On macOS **the helper is the privilege boundary**. Fusing it into
the Deck would put the GUI, the Metal renderer, the video decoder, the QUIC
transport, and the settings surface inside a root process image. The lab build
that ran the whole app as root proved the capability and was never a shippable
shape.

Its rules are the ones any future helper should inherit:

- Registered with `SMAppService`. `SMJobBless` is deprecated;
  `AuthorizationExecuteWithPrivileges` is forbidden.
- Callers pinned with `NSXPCListener.setConnectionCodeSigningRequirement`, plus
  an audit-token check. Authenticating by pid is forbidden — that is the
  CVE-2019-8526 pid-reuse class.
- The helper re-derives policy from descriptors it read itself. It never trusts
  a device claim from the Deck.
- It carries `arcen-protocol` frames, so the macOS and Linux privilege
  boundaries validate the same shapes under the same bounds.

## A webcam: which route?

Nothing exists yet. Both routes are open, and they are not equivalent.

**Consent route — capture with AVFoundation.** The Deck asks for camera access
the same way it asks for the microphone, receives frames, and forwards them.
No helper, no root, no new privilege boundary. The host sees a virtual camera fed
by Arcen rather than the physical device.

This is very likely the right first implementation, and one detail makes it more
attractive than it first appears: **USB video cameras usually emit compressed
frames already.** The UVC specification carries MJPEG and frame-based formats as
well as uncompressed YUY2, and most cameras above the cheapest tier offer at
least MJPEG. If AVFoundation can be persuaded to hand over that native stream
rather than decoded frames, Arcen forwards bytes the camera already produced —
no decode, no re-encode, no added latency, and a fraction of the bandwidth of
raw frames.

*Unverified.* Whether AVFoundation exposes the native compressed stream, and
under which capture presets, has not been tested. That measurement is the first
task for anyone taking this on, because it decides the whole shape.

**Device-capture route — pass the USB camera through.** The host binds its own
driver and sees the real device, exactly as with Hard USB. This is the only
option if a remote application demands a specific camera model, or needs vendor
controls the standard path hides.

The cost is real: root, a second helper or an extension of the existing one, and
a webcam's bandwidth is far higher than a tablet's. It should not be attempted
until the consent route has been shown insufficient for a concrete need.

**Recommendation.** Start with consent and AVFoundation. Measure whether the
compressed stream is reachable before designing anything. Treat device capture
as an escalation with a named reason, never a default.

## The rule this leaves behind

Reach for consent first. Escalate to device capture only when a specific
capability is demonstrably unreachable any other way, and when it escalates, put
only the escalation in the privileged process.

The Deck should never run as root. If a capability seems to demand that, the
design is wrong, not the platform.
