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

Amendment (2026-11): the desktop's bridge class now sets the driver's
`RawSample` to 1 (`arcen_usb_bridge::bridge_seat::XORG_BRIDGE_RAW_SAMPLE`),
so the X pointer, and the ink that follows it, is at the pen's latest
report rather than the average of its last four. `Suppress` keeps the
driver's default (2 tablet units). A desktop an earlier Pier started ends
with that Pier's class, which confines bridged devices the same way; it is
still recognised (`admitted_bridge_display`), so it never blocks another
desktop's Native tablet.

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

## Amendment 2026-11-15: any tablet by default; Tablet support and Native tablet are different contracts

**Status:** Accepted (2026-11-15). Amends
[ADR 0022](0022-macos-pier-native-tablet-hid-adapter.md) §14.2 and
[ADR 0021](0021-bridged-usb-devices-on-their-own-seat.md)'s X11 driver
requirement.

### Rule: any device by default

Arcen supports any pen tablet by default, to the best of its ability, on
every Deck and every Pier, as game-streaming clients support any controller
rather than a list of known ones. A dedicated reader exists only where it
adds something a device's own description does not say; it is never the
gate. A refusal names what is genuinely unsafe or genuinely impossible, with
the device's ids and the alternative; "not a model we know" is not a reason.

| Where | Before | After |
| --- | --- | --- |
| macOS Pier, Native tablet | Only `056a:0317` (Intuos5 touch L) was read; every other model refused (ADR 0022 §14.2) | A known model keeps its dedicated reader (`IntuosPenReader`: Wacom mode switch, tool id, eraser end). Any other tablet is read by its pen interface's own HID report descriptor (`arcen_usb_bridge::generic_pen`, chosen by `host_pen_route`). Refused only when its pen speaks a vendor format no reader knows (an Intuos-family packet of an unknown model is one) |
| macOS Pier, publication | A pen interface that also declared a keyboard or mouse was withheld, so its pen could not be read | Such an interface is polled for the Pier alone (`HidHostAdapter::with_host_read_interface`) and still never published; with nothing publishable no device owner starts |
| Linux Pier, X11 desktop | The pen had to be served by the kernel `wacom` driver | Any node udev marks as a pen, whichever kernel driver serves it (`hid-generic`, `hid-uclogic`, ...); xf86-input-wacom is still required: it is upstream's X driver for every evdev tablet (`MatchIsTablet`) |
| Linux Pier, native-seat Wayland | Model-agnostic already (the kernel and libinput bind) | Unchanged |
| Windows Deck, USB component | "Prepare this tablet" was offered only for a Digitizers-page collection | Any collection that names a pen on the Digitizers page or its vendor mirror `0xFF0D` (`names_pen_digitizer`); still prepared per model and claimed per session |

Refusals kept, each with its reason: a headless Wayland desktop (no seat to
confine a USB device, `admit_hard_usb_session`); a keyboard interface on an
X11 desktop (the console keyboard handler would take it); the login window
and another account's console on a macOS Pier; game controllers and
non-pen devices (`classify_pen_tablet`).

Native tablet on a **Windows Pier** (implemented for 0.20.0, 2026-11-16).
ADR 0017 declined the route that needs a virtual USB host controller (a
kernel driver Arcen would have to sign). The macOS Pier's design is another
route that needs no driver, and the Windows Pier takes it: it publishes no
device (`HidPublishPolicy::Nothing`), reads the forwarded pen with this
amendment's readers (dedicated or generic) and injects the samples through
the synthetic Windows Ink pen it already owns (`PenInjector`,
`InjectSyntheticPointerInput`), in its own session only. The attachment is
one shared implementation for both Piers that read the pen themselves:
`arcen_usb_bridge::HostPenBridge` (the HID host adapter, the reader, the
Intuos mode switch and its restore, the proximity release) and
`arcen_input::bridged_pen` (authorization, the Deck's wording, wire frames,
the host pointer). The macOS Pier adds only its device-owner child and
console watch, the Windows Pier only the injection. It is offered whenever
the session's Windows Ink pen device exists; touch, the tablet's keys and
the maker's driver settings are not used there, as on a macOS Pier.

### The generic reader

`GenericPenLayout` is parsed from one interface's report descriptor:
for each input report inside a pen application collection, the bit
position, size and logical/physical range of X and Y, tip, in range,
pressure, barrel and secondary barrel, invert and eraser, X/Y tilt and
twist, on the Digitizers page, Generic Desktop, the Button page (only where
the Digitizers usage is absent) and the vendor digitizer convention
(`0xFF0D`, high byte of the usage naming the standard page). Every report
of that interface then becomes the same `HostPenSample` the dedicated
reader produces, so the tablet event, the shared pen sample of this ADR and
the Deck's pen dot are unchanged.

The descriptor and every report come from the Deck and are untrusted: the
walker has the classifier's bounds (4096 bytes, nesting 32, 256 usages per
item, global stack 16), fields are at most 32 bits, a report shorter than
its layout is not read, and every value is clamped into its declared range
before it becomes an event. Deterministic fuzz tests run random and mutated
descriptors and reports; authored fixtures cover Huion-, Gaomon-, XP-Pen-,
Wacom-generic- and Windows-style layouts. A lab emulation of a generic pen
uses the pid.codes test identity `1209:000F` (in-house testing only, never
shipped).

### Tablet support and Native tablet are different contracts

- **Tablet support** is the operating systems' own pen stacks on both ends:
  the Deck takes the pen from its native stack (AppKit tablet events on a
  Mac; `WM_POINTER` pen, that is Windows Ink, on a PC) and the Pier injects
  it through its native pen path (CoreGraphics tablet events on macOS, the
  synthetic pointer pen on Windows, the uinput or libei stylus on Linux).
  Whatever the Deck's operating system and driver accept works, and
  handedness, area and pressure mapping are theirs. The Deck does not
  duplicate them: the Mac's "Tablet Orientation Left-handed" item (which
  mirrored X and negated X tilt of every sample) and its unused "Force
  Proportions" state are removed; on Windows the same shared item did
  nothing and is gone too. A left-handed user sets the tablet's orientation
  in its driver, as for every other application on that machine.
- **Native tablet** is USB passthrough of the real device. The host's
  driver (Linux) or the Pier's reader (macOS) interprets it; the Deck's
  operating system loses the tablet for the session.

### Consequences

- A tablet nobody at Arcen has held works on a macOS Pier with pressure,
  tilt, twist, barrel buttons and eraser, as far as its descriptor states
  them; one in a vendor format gets a clear refusal and keeps Tablet support.
- X11 hosts accept non-Wacom tablets; how well xf86-input-wacom drives an
  unknown tablet is its own (it treats an unknown USB tablet as a generic
  model).
- The Decks' Native tablet text says "Any pen tablet works; models with a
  dedicated reader get extra features" and states per host what differs.
