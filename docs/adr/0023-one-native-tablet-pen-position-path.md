# ADR 0023: One Native tablet pen-position path for every Pier

**Status:** Accepted (2026-11-12). Implemented on the Linux Pier (X11
and native-seat Wayland) and the macOS Pier; the Windows adapter is a
documented follow-up. Amends [ADR 0020](0020-linux-native-tablet-in-tree-vhci-hcd.md),
[ADR 0021](0021-bridged-usb-devices-on-their-own-seat.md) and
[ADR 0022](0022-macos-pier-native-tablet-hid-adapter.md).

> Scope: the public API in
> `arcen-input` (`tablet_pointer::{PenSample, EvdevPenFrames,
> PenPointerLead, PenMapping, x11_wacom_mapping}`,
> `host_pointer::HostPointerFeed`) and the changed rendering rule
> (`host_pointer::led_pointer_mark`). No wire change.

## Context

With Native tablet the tablet is attached to the host, so only the host
knows where the pen is, and a Deck that draws its own cursor (Local cursor)
must be told (`CursorPosition`, ADR 0014 amendment 21). By 2026-11-12 three
Piers did that three ways:

| Pier | Source of the pen position | What the Deck drew |
| --- | --- | --- |
| Linux, Wayland | the bridged pen's evdev node, read as each report arrived (`usb_bridge::tablet_pointer`, Wayland-only) | a pen mark (GNOME hides its pointer while a tool is in proximity) |
| Linux, X11 | `XQueryPointer` polled every 8 ms by the cursor watcher, after `xf86-input-wacom` had processed the report | the application's X cursor image |
| macOS | the Pier's own Intuos pen reader, with its own pixel mapping (`round(fraction × (size − 1))`) and its own reporter wrapper | the macOS cursor image |

A real Intuos5 was tried on Linux X11 and Wayland Piers with the same Deck
(a 0.19.0 development build): on Wayland the pen dot did not lag; on X11 it
still felt laggy.

## Decision

One path, with thin platform adapters that only produce samples:

1. **Sample.** `arcen_input::tablet_pointer::PenSample`: the tool in
   proximity (pen, eraser, other, or none), x/y in device units with their
   ranges, and the time the adapter read it.
2. **Adapters.**
   - Linux (X11 and Wayland, the same code): `usb_bridge::tablet_pointer`
     opens the bridged pen's own evdev node read-only, never grabbed,
     `O_NOFOLLOW`, proves the open file is the verified device (character
     device, the device number sysfs gives the verified node, which still
     carries its seat's bridge rule, re-verified after the open), and turns
     its events into samples with the shared `EvdevPenFrames` (one sample per
     `SYN_REPORT` that changed something; `SYN_DROPPED` resynced from
     `EVIOCGKEY`/`EVIOCGABS` as libevdev does).
   - macOS: `bridged_tablet::host_pen_sample` converts each read of the
     shared `IntuosPenReader` (device units over the model's range). The
     reader's fractions for the injected tablet events use the same scaling,
     so the event and the reported position land on the same pixel.
   - Windows: no adapter. Windows has no Native tablet (ADR 0017); an
     adapter would produce `PenSample`s from its pen source and feed the same
     lead. Follow-up only if ADR 0017 is revisited.
3. **One policy.** `PenPointerLead`: the pen leads while a tool is in
   proximity **and** the desktop maps the whole tablet to the whole desktop
   (`PenMapping::WholeArea`); its position is `floor(fraction × size)`, the
   X server's and Mutter's own scaling (`(v − min) / (max − min + 1)`).
   Otherwise the platform's own pointer leads: the X pointer (cursor
   watcher) on X11, GNOME's core pointer (capture metadata) on Wayland. When
   the pen leaves, nothing is published (the cursor stays where the pen left
   it); a mapping that stops being the whole area hands the pointer back at
   once.
4. **Mapping proof.** GNOME on Wayland and the macOS Pier map the whole
   area by construction (GNOME's defaults; the macOS Pier posts the events
   itself). An Xorg desktop proves it: the cursor watcher reads, once a
   second, every `xf86-input-wacom` device of the pen's node (matched by its
   "Device Node" `stat`'s device number) and requires "Wacom Tablet Area" =
   the evdev range, "Wacom Rotation" 0, an identity "Coordinate
   Transformation Matrix" and absolute mode (`x11_wacom_mapping`). Until the
   X server has added the pen, the mapping is `Unknown` and the X pointer
   leads.
5. **One feed.** What the lead publishes is a `DesktopPointerSample`, fed to
   `host_pointer::HostPointerFeed` (echo suppression, coalescing, 120 Hz
   rate, lane discipline, the `CursorPosition` record) on every Pier.
6. **One mark.** While Native tablet is active the Deck draws the pen mark
   (white dot, dark ring) at a host-led position, on every host and whatever
   the host's cursor image is (`led_pointer_mark`). The Mac's own mouse
   takes the cursor back the moment it moves.

### What the X pointer is still for

The X11 cursor watcher keeps reading `XQueryPointer` (8 ms while it moves,
33 ms at rest) for what no pen report shows: an application's pointer warp
(core warps generate no input events), a tablet the user remapped (then only
the X server knows where the pen lands), and any session without a bridged
pen (Local cursor with host positions). It is never read while the pen leads.
The 0.19.0 "bridged" mode that polled every 8 ms even at rest is removed: the
pen no longer depends on the poll.

### Removed

| What | Where |
| --- | --- |
| The Wayland-only pen reader's sink and the hub's own pen/core-pointer bookkeeping | `media::wayland_cursor::WaylandCursorHub::apply_tablet`, `stream_size`, `core_pointer` |
| The Wayland-only reader gate | `usb_bridge::NativeSeatPort` (now `BridgePort`, X11 and Wayland), `#[cfg(feature = "wayland-provider")]` on the reader |
| X polling as the pen's source, and its always-8 ms mode | `cursor_watcher::PointerReading::{Off, Adaptive, Bridged}` |
| The evdev-only update type | `tablet_pointer::{TabletPointerTracker, TabletPointerUpdate}` → `EvdevPenFrames`, `PenSample` |
| The macOS Pier's own pixel mapping | `bridged_tablet::HostPenPointer::observe` (pixel math from `PenEventMsg`) |
| The duplicated feed in the Linux lane worker | `lane_workers::PointerReporting` layout code → `HostPointerFeed` |

## Measured

Emulated Intuos5 (`--usb-emulate-stroke realistic`, four strokes per run)
from the Deck CLI on a Mac over a VPN, into a Pier on a Rocky Linux 9 X11
host with the packaged configuration. `base` is a build that follows the X
pointer (X poll), `new` a build with this change (evdev). The
evdev time is the kernel's timestamp on the pen's report; "sent" is the
Pier's own time-stamped trace line; "received" is the Deck's line time
corrected by the Mac-to-host clock offset (measured over one ssh session
before and after each run, the smallest of 40 round trips; it can be off by
up to half that round trip, the same for both variants).

"Behind the pen" places every reported position on the pen's own path
(its evdev reports joined in time) and takes how long ago the pen was
there; it also covers positions no single report produced.

| Run (one pen report every …) | Path | Behind the pen when sent, median (p90) | Behind the pen at the Deck, median (p90) |
| --- | --- | --- | --- |
| 41.5 ms (2 runs each) | X poll (0.19.0) | 63.6 / 65.8 ms (77 / 71) | 79.9 / 81.9 ms (93 / 88) |
| 41.5 ms (2 runs each) | evdev (this change) | 0.2 / 0.2 ms (0.5 / 0.5) | 16.1 / 16.6 ms (18 / 20) |
| 82–102 ms (congested link) | X poll | 126.7 ms (160) | 146.6 ms (179) |
| 82–102 ms (congested link) | evdev | 0.2 / 0.2 ms | 21.1 / 36.4 ms (clock offset uncertain: round trip 41–80 ms) |

Why the X pointer trails by about one and a half pen reports, not 8 ms:
`xf86-input-wacom` averages raw samples ("Wacom Sample and Suppress" 2, 4,
its default) before it moves the pointer, and a bridged tablet delivers one
report per network round trip, so the average spans several round trips.
Where a polled position did equal a report's own pixel (8 of 128 per run,
at stroke starts) it was sent 2–6 ms after the report; the trail is the
averaging. Sent-to-received is the same for both paths (about 16 ms, half
the round trip). The application's ink on X11 still follows the averaged X
pointer; the Deck's dot now shows where the pen is.

A remap was proven to hand the pointer back: setting the stylus's
"Coordinate Transformation Matrix" to `0.5 0 0 0 1 0 0 0 1` mid-run gave
`Remapped(Transform)` 0.86 s later, and "Wacom Rotation" 3 gave
`Remapped(Rotation)` 10 ms later; the positions then follow the X pointer.
The default properties on the test host were "Wacom Tablet Area" `0, 0, 65024,
40640` (the evdev range), "Wacom Rotation" 0 and an identity matrix, and the
mapping was proven `WholeArea` 34 ms after the reader started.

## Consequences

- The X11 pen feels like the Wayland pen: the position leaves the Pier when
  the report arrives, not after the X server and the next poll.
- One rendering rule: the user sees the same dot on X11, Wayland and macOS.
  The application's cursor shape (an I-beam, a crosshair) is no longer shown
  at a pen-led position; the Mac's own mouse still shows it.
- A remapped X11 tablet still works; its dot follows the X pointer (the old
  latency) and the Pier logs the mapping it found.
- GNOME on Wayland is assumed to use its default mapping; reading
  `org.gnome.desktop.peripherals.tablet` from the session bus is a
  follow-up.
- A source flag on the wire (`CursorPosition` "pen leads") would let the
  Deck tell pen from application warps; not needed while Native tablet
  makes the pen the only host-side pointer, and it would need a negotiated
  protocol change (old Decks reject unknown flag bits).

## Lessons

1. Physical-tablet and real-browser validation complements emulation: an
   emulated pen can pass where a real tablet fails, so every pen change needs
   a real-tablet or real-browser proof.
2. A hung or held side switch looks exactly like "pen but no ink".
3. Event-driven evdev positions beat X polling for perceived latency.
