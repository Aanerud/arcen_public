# Trackpad gestures: what is deliverable and what is not

Research verified against the macOS 26.5 SDK headers and vendor documentation,
September 2026. Written down because the central fact is counter-intuitive and
will otherwise be rediscovered by whoever tries this next.

## The asymmetry

macOS has a **rich, fully public capture API** for gestures and **no public
injection API for them at all**.

`CGEventType` stops at `kCGEventOtherMouseDragged`. There is no
`kCGEventMagnify`, no rotation or gesture-phase key in `CGEventField`, and the
structures needed to hand-build such an event live in `IOHIDEventData.h`, which
is not in the public SDK. Every shipping tool that synthesises a macOS pinch
splices an undocumented binary blob into a serialised `CGEvent`. That is
private ABI. Arcen ships under Developer ID and will not do it.

Linux is the mirror image: XInput 2.4 delivers real pinch and swipe events, and
Wayland has `zwp_pointer_gesture_pinch_v1` — but both are **delivery-only**,
with no injection path. The supported route is one layer lower: a virtual
multi-touch touchpad via `uinput`, which libinput then recognises as a gesture.
Windows is the same shape — `WM_GESTURE`/`GID_ZOOM` for consumption,
`InitializeTouchInjection` + `InjectTouchInput` for synthesis.

**No mainstream remote desktop transmits semantic gestures.** RDP (MS-RDPEI)
and Chrome Remote Desktop ship raw contacts and re-recognise on the host. VNC
ships nothing and degrades a pinch to synthetic Ctrl+wheel.

## The matrix

| Gesture | Capture on the Deck | → macOS Pier | → Linux Pier | → Windows Pier |
| --- | --- | --- | --- | --- |
| Two-finger scroll, phased and inertial | **public** | **public** | public | public |
| Pinch / magnify | **public** | **private only** | public via `uinput` | public via touch injection |
| Rotate | **public** | **private only** | public via `uinput` | public via touch injection |
| Smart magnify (two-finger double tap) | **public** | private only | no equivalent | no equivalent |
| Two-finger swipe (between pages) | **public** | private only | fallback only | fallback only |
| Three/four-finger swipe | **not capturable** | — | public via `uinput` | not possible |
| Raw finger positions | **public** | private only | public | public |
| Force click / pressure | **public** | not possible | not possible | not possible |

## What to build, in order

1. **Phased, pixel-precise, inertial scroll — including horizontal.** This is
   the highest-value item and it is fully public at both ends. Arcen already
   carries scroll, but without `phase` and `momentumPhase` the remote feel is
   measurably worse than local: no rubber-banding, no inertia, no clean end of
   a flick. `NSEventPhase` on capture maps 1:1 onto
   `kCGScrollWheelEventScrollPhase` (99), `kCGScrollWheelEventMomentumPhase`
   (123) and `kCGScrollWheelEventIsContinuous` (88) on injection.

   This alone will do more for how the desktop feels than gestures will.

2. **A gesture-aware wire representation**, carrying
   `{kind, phase, scale_or_delta, rotation_degrees, anchor, finger_count,
   timestamp}` in `shared/protocol`, with the decision of how to realise it
   left to each Pier. This costs little, and it means a future public macOS API
   is a Pier-side change rather than a protocol change.

3. **Pinch and rotate to a Linux Pier**, through a `uinput` virtual multi-touch
   touchpad. Every layer is public: kernel uinput, kernel MT protocol type B,
   libinput's recogniser, XI 2.4 delivery. It is re-synthesis, so thresholds
   and timing will not match the Deck exactly.

4. **Pinch and rotate to a Windows Pier**, through `InitializeTouchInjection`.
   Windows re-runs recognition. This is *touchscreen* touch, so it reaches
   touch-aware applications and not legacy wheel-only Win32 ones.

## What not to promise

- **Pinch or rotate to a macOS Pier.** Not possible under Developer ID. This is
  the one true dead end, and it is the macOS-to-macOS case.
- **Smart magnify anywhere but macOS.** No equivalent concept exists in
  libinput, XI 2.4, Wayland or Windows.
- **Three- and four-finger swipes from the Deck.** The system consumes them
  before an application sees them.
- **Force click or pressure injection anywhere.**

## On the Ctrl+scroll fallback

Translating a pinch into Ctrl+scroll is defensible only as an **explicit,
user-visible, opt-in setting**, and never as a silent default:

- On a macOS host it is as likely to trigger the *operator's own* Accessibility
  Zoom as the remote application's.
- On Windows it silently scrolls in applications that ignore `MK_CONTROL`.
- It is structurally invisible to exactly the modern Qt and GTK4 applications
  most likely to support zoom properly, because they implement zoom through
  the native-gesture path rather than by watching for a modifier.

If it is implemented, two details are worth taking from noVNC and TigerVNC:
send a pointer move first, because some protocols ignore position on wheel
events; and guard the synthetic Ctrl so it cannot leak into the host's modifier
state if the connection drops mid-gesture.

## Capture details worth knowing

- `NSEventTypeBeginGesture` (19) and `NSEventTypeEndGesture` (20) are dead.
  Applications linked against 10.11 and later no longer receive them; use
  `NSEvent.phase` on the magnify or rotate event instead.
- `NSEvent.magnification` is a **delta to add** to the current scale, not an
  absolute scale factor.
- `NSEventTypeSwipe` carries discrete ±1 in `deltaX`/`deltaY`, not a continuous
  value, and is gated on `isSwipeTrackingFromScrollEventsEnabled`.
- Raw fingers via `NSEvent.touches(matching:in:)` are **normalised**, not in
  screen space, and require `allowedTouchTypes = .indirect`. Apple is explicit
  that touches have no corresponding screen location.
- Capture needs **no entitlement and raises no TCC prompt**. It is ordinary
  event delivery to the application's own key window, and works under App
  Sandbox and Hardened Runtime.
