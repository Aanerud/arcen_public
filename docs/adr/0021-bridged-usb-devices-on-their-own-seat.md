# ADR 0021: Bridged USB tablets on their own seat; X11 desktops take them as static devices

**Status:** Accepted; shipped in 0.19.0 (first proposed 2026-11-10 for lab
builds). Implemented
for dedicated Xorg desktops on the Linux Pier and lab-proven on Rocky Linux 9
(Xorg 1.20, GDM greeter on `seat0`) with an emulated Wacom Intuos5, over both
the in-tree `vhci-hcd` and the lab `usb-vhci` controller. Native-seat Wayland
desktops use the same seat rule with their own seat
([ADR 0020](0020-linux-native-tablet-in-tree-vhci-hcd.md)); see the amendment
of 2026-11-12 for their pen contact and the Deck's cursor.

**Amended 2026-11-11 (first-connection hotplug):** Decisions 2–4 below are
superseded by the amendment at the end of this record. A dedicated Xorg
desktop now hot-adds the bridged tablet on the first connection; the private
seat, the static devices, parking, state replay and the sign-out step are
gone. Decision 1 still holds for native-seat Wayland desktops.

**0.19.0 status note (2026-11-12):** shipped in the public Linux Pier behind
the administrator opt-in `usb.native_tablet` (see
[ADR 0020](0020-linux-native-tablet-in-tree-vhci-hcd.md)'s status note). The
security model of this record is what an administrator enables: only a
device classified as a pen tablet is attached, only to the signed-in user's
own desktop (X11: tagged for that X server alone; native-seat Wayland: that
desktop's seat), never to `seat0`'s console or another user's session.
Proven with a real Wacom Intuos5 bridged from a Mac Deck, on a Rocky Linux 9
X11 host and a Rocky Linux 10 GNOME Wayland host: hot-plug on the first
connection, the Deck-drawn pen dot, drawing, hand-back. Drawing in browsers
needed `PressureRecalibration` off (see the 41 ms measurement below), first
proven with replayed pressing landings in Firefox and Chrome. Native-seat
Wayland is also proven
with the emulated Intuos5 (drawing with pressure in Firefox and Chrome on
Rocky 10 GNOME; the Pier reads the bridged pen's evdev node read-only so
the Deck's cursor follows it). Known limits there: Mutter squares stylus
pressure on its default curve, a tablet remapped in GNOME Settings offsets
the Deck's cursor, and a multi-output desktop reports no cursor positions.

**Amended 2026-11-12 (one pen-position path):** the native-seat Wayland
reader of the amendment below is now the one Linux pen adapter, for
dedicated Xorg desktops too ([ADR 0023](0023-one-native-tablet-pen-position-path.md)).
On X11 the Deck's pen dot follows the pen's own evdev reports while the pen
is in proximity and `xf86-input-wacom` maps the whole tablet (checked from
the pen's X device properties); the X pointer leads otherwise. When the pen
leaves proximity nothing is reported (on Wayland the core pointer's position
is no longer re-sent); the Deck's dot stays where the pen left.

> Scope: where a bridged device's input nodes go (without `uaccess`),
> volatile udev rules under `/run`, an input path into a dedicated Xorg, and
> the public API in `arcen-usb-bridge` (`bridge_seat`, `DeviceStateReplay`,
> the `HardUsbSessionSeat` variants).

## Context

Native tablet (the USB bridge) presents the Deck's tablet on a virtual USB
host controller, so the host kernel's own driver (for example `wacom`) binds
it. Its input nodes are then ordinary udev devices, and udev puts them on
`seat0`. Since 0.17.0 a dedicated Xorg desktop admits only its own session's
virtual devices (the isolation classes of `with_session_input_isolation`), so
the session ignored the bridged tablet while GDM's greeter Xorg, also on
`seat0`, took it. That broke Native tablet on X11 and let a remote device reach
the login screen.

Three facts constrain the fix:

1. A `seat0` Xorg hot-adds only `seat0` devices (xserver 1.20
   `config/udev.c check_seat`); `Xorg -seat` is not viable on the shared GPU.
2. A static `InputDevice` section is opened when Xorg starts, and
   xf86-input-libinput looks a path up by its exact udev device node, so a
   section cannot be pointed at a device that appears later (lab-tested:
   symlinks and placeholders fail with "Failed to look up path").
3. The Deck names its tablet in its hello, which follows the Pier's hello,
   which follows the desktop's creation. The Pier therefore never knows the
   tablet before a desktop's Xorg starts.

## Decision

1. **A bridged device is never on `seat0`.** Before a virtual port is
   connected, the privileged helper installs a volatile rule,
   `/run/udev/rules.d/72-arcen-usb-bridge-<id>.rules`, that matches exactly
   that port's subtree by `DEVPATH` (`vhci_hcd.N` first, `usb_vhci_hcd.N` as
   the lab fallback) and sets `ID_SEAT` to an Arcen seat, removes `uaccess`
   and keeps the device powered. It also gives every event node a link that
   carries the bridge's random seat id,
   `/dev/input/arcen-bridge-usb-<id>-eventN`, which udev removes with the
   device and which no other bridge or device ever gets. The rule is removed
   after the port's subtree is gone. The Pier proves every node from the udev database
   (`E:ID_SEAT=<seat>`, no `uaccess`) before anything uses it, and refuses
   otherwise. Rules whose owning helper died are swept at Pier start, and the
   installer removes any left at uninstall. Before a helper installs its rule
   it removes any rule for the same port whose owner is dead (a helper killed
   without cleanup), so a new bridge never matches an older seat's rule; a
   live owner's rule for the port refuses the install.
2. **X11: a private seat and static devices.** For a dedicated Xorg desktop
   the seat is `seat-arcen-usb-<16 hex>`, a seat nobody runs (no master, no
   logind seat object). The desktop's launcher verifies each node again
   (seat, no `uaccess`, and the bridge's link naming exactly that node) and
   renders static xf86-input-wacom `InputDevice` sections by tool type
   (stylus, eraser, pad, touch) for the nodes the kernel `wacom` driver
   serves, each opened through **the bridge's link, never
   `/dev/input/eventN`**. A kernel event number is reused by the next input
   device: a desktop that outlived its bridge and reopened `eventN` (a root
   Xorg reopens a device on `xinput enable` or a VT return) could otherwise
   read another session's or the console's keyboard. Once the bridge ends,
   its links are gone and a reopen fails. xf86-input-libinput is never used
   for these sections: it opens only the exact udev device node (not a link)
   and, on Xorg 1.20, cannot create pen tool subdevices for a
   configuration-file device. A tablet without such a pen node is refused on
   X11. The isolation classes are unchanged.
3. **The bridge outlives attachments, and the first one prepares it.** An
   attachment to a desktop that was started without the tablet brings the
   tablet up on its private seat and, once its nodes are verified, ends Native
   tablet for that attachment (the Deck gives the tablet back to the Mac, and
   the pen works as a mouse there; the Deck is told to sign out and in) and
   parks it for the account's next desktop. The hand-back waits until no
   control transfer is outstanding and control traffic has been quiet for a
   moment (capped), because the host driver's mode switch follows probe. If
   preparation refuses the tablet (no X11 pen node, or a failed
   verification), the Deck gets the same hand-back with the reason and the
   session goes on; the refusal is remembered for that tablet until the X
   wacom driver's installed state changes. A downgrade to Tablet support is
   not possible without a Deck change: the Deck treats a Native request
   answered with Tablet support as inconsistent. The next desktop of that account is started with
   the parked tablet's nodes. Every attachment of the same tablet (same
   identity and report descriptors) is served by the same bridge; between
   attachments interrupt transfers wait and control transfers stall, and the
   state-setting requests the host driver made (`SET_CONFIGURATION`, class and
   vendor OUT requests such as Wacom's mode switch) are replayed to the
   re-captured device first. Another tablet is refused with a reason.
4. **Teardown.** A bridge ends when its desktop has ended and it stayed parked
   past `ARCEN_USB_BRIDGE_PARK_SECS` (default 600 s; a bridge prepared by a
   still-living desktop waits for it), when its helper ends, or at Pier stop,
   where the helper disconnects the port, waits for the subtree to go and
   removes its rule. The Pier never kills a helper for dropping its handle:
   it closes the helper's input, which ends the helper the same clean way,
   and at stop it waits (bounded) for every helper to finish. A helper
   killed between disconnect and rule removal would leave its rule behind.

## Consequences

- Requirement (e) holds by construction: the console seat's devices never
  reach a remote desktop (classes unchanged), and a bridged device never
  reaches GDM or `seat0` (rule before connect, verified nodes, no
  `uaccess`). Tests cover the rule, the verification, the launcher rendering
  and the installer cleanup.
- Native tablet on X11 needs one sign-out and sign-in after the tablet is
  first used with a desktop. Removing that step needs the Deck to name its
  tablet before the desktop exists (an authentication-time field), which is a
  protocol change left for later.
- A Pier restart, a helper failure or a guard teardown ends the bridge
  before its desktop: the desktop's static devices are then dead (their
  links are gone, so they cannot reopen anything), and Native tablet is
  refused on it ("needs a new desktop").
- Native tablet on X11 is for tablets the kernel `wacom` driver serves, with
  xf86-input-wacom installed; others are refused with a reason.
- **Follow-up:** removing the sign-out step without a protocol change would
  need the Pier to enumerate a remembered tablet itself before the Deck
  attaches (answering standard requests from recorded descriptors, holding
  interrupt transfers). Not attempted; the cleaner route is an
  authentication-time device field.
- A re-captured device whose state is not set by control requests, or that a
  re-capture resets in other ways, may need a new desktop.

## Amendment (2026-11-11): X11 desktops hot-add the tablet on the first connection

The "prepare, hand back as a mouse, sign out and in" flow was rejected:
Native tablet must work on an X11 desktop on the first connection.

### Facts (xserver 1.20.11 source, `config/udev.c`, `xf86Xinput.c`, `xf86Config.c`)

- `check_seat` lets a `seat0` server hot-add every device whose `ID_SEAT` is
  unset or `seat0`; it is the only silent filter.
- `MatchTag` compares whole tags against the udev property
  `ID_INPUT.tags` (comma-separated), not udev's `TAGS`.
- `IgnoreInputClass` returns the value of the last matching class, and it
  runs before the server opens the device (before `systemd_logind_take_fd`
  and before a driver loads). A later class can therefore un-ignore.
- Configuration is read in this order: `/usr/share/X11/xorg.conf.d`, then
  `/etc/X11/xorg.conf.d` (also with `-config`, unless `-configdir` is given),
  then the `-config` file. A dedicated desktop's own classes come last.
- A server reads its configuration once, at start.
- libinput refuses any device with `LIBINPUT_IGNORE_DEVICE=1`.

### Decision

1. For a dedicated Xorg desktop on `:N`, the bridge's per-port rule (still
   written before the port connects) keeps the subtree on `seat0` (by
   `ID_SEAT`, which is what Xorg checks), strips `uaccess` and logind's
   `seat` and `power-switch` tags (so logind neither tracks the nodes as
   `seat0` devices for a console session to take nor acts on their power
   keys), and sets on every input node
   `ID_INPUT.tags=arcen_bridge,arcen_bridge_N` and `LIBINPUT_IGNORE_DEVICE=1`.
2. The installer installs `/etc/X11/xorg.conf.d/90-arcen-bridged-usb.conf`:
   `MatchTag "arcen_bridge"`, `Ignore on`. Every X server (GDM's greeter,
   console sessions, other desktops) ignores bridged devices before opening
   them. libinput compositors (a Wayland greeter or console session) ignore
   them by the property.
3. The launcher appends, last and after the unchanged isolation classes,
   two classes: `MatchTag "arcen_bridge"`, `Ignore on`, then
   `MatchTag "arcen_bridge_N"`, `Driver "wacom"`, `Ignore off`,
   `PressureRecalibration off` (see the amendment of 2026-11-11 on pen
   contact). The first is
   needed because the isolation class `arcen-session-input` re-admits by
   USB vendor id (`a2ce`) and product name (`(session :N)`), and a bridged
   device's string descriptors and ids are chosen by the Deck: without it,
   another desktop's server would re-admit a bridged device that imitates its
   virtual devices. The desktop hot-adds its own tablet the moment it
   connects; xf86-input-wacom creates its stylus, eraser, pad and touch
   devices.
4. Fail closed before any bridge starts: the Pier checks that the ignore
   file is installed, that the desktop's configuration ends with exactly its
   own two bridge classes, and that every other running X server either is
   another dedicated desktop whose configuration ends with its own bridge
   classes, or is not a dedicated desktop and started after the file was
   installed (without `-configdir`) (`arcen_usb_bridge::bridge_seat::x11_isolation_in_force`).
   Otherwise Native tablet is refused with a reason that names the fix
   (restart the display manager). After the tablet connects, every node is
   verified from the udev database (tags, `LIBINPUT_IGNORE_DEVICE`, no
   `uaccess`, no seat but `seat0`), and a tablet whose pen the kernel `wacom`
   driver does not serve, or with xf86-input-wacom missing, is refused. A
   refusal stops Native tablet with the reason. The session goes on, and the
   reason is never a sign-out instruction.
5. The bridge belongs to the attachment: disconnecting ends it, and the
   desktop's server drops the devices. Reconnecting starts a new bridge.
6. Pens, pads and touch only. On `seat0` the kernel's console keyboard
   handler (text VTs, SysRq) binds every input device with keyboard keys,
   whatever udev or Xorg decide, and `hid-input` maps keys per field usage
   whatever the collection. A tablet with any Input item outside an
   allowlist (Digitizers and Button pages; Generic Desktop Pointer, Mouse,
   X, Y, Z, Rx, Ry, Rz, Slider, Dial, Wheel; the vendor pages `0xFF00`, which
   `hid-input` ignores, and `0xFF0D`, which it maps to `BTN_MISC`/`ABS_MISC`
   at most; not `0xFF01` or `0xFF7F`, which it maps to keyboard keys) in any
   collection at any depth of its authorized report descriptors is refused
   before its port connects. Usages are paged as Linux 5.3+ pages them at
   each Main item (`hid_concatenate_last_usage_page`), and a Usage Page
   change between a usage and its Main item is refused outright; and a bridged node whose key capabilities hold a key
   the handler binds (below `BTN_MISC`, or a braille dot) ends the bridge
   with a reason. The Intuos5's pen, pad and touch nodes carry buttons only.
   This was already possible with the private seat (the handler ignores
   seats); it is closed here for X11.

### Consequences

- Requirement (e) holds: console devices still meet the isolation classes,
  and no X server but the desktop's own opens a bridged device. GDM's log
  does record `Adding input device …` and then `Ignoring device from
  InputClass "arcen-bridged-usb-ignore"`, because the server ignores a
  device only after it has seen it.
- An X server that was running before the ignore file was installed must be
  restarted once (`systemctl restart gdm` when nobody is at the console, or
  a reboot). Until then the Pier refuses Native tablet on X11.
- A desktop an earlier Pier started does not admit the tablet. It is refused
  with a reason; desktops this Pier starts take it.
- Native tablet on X11 still needs a tablet the kernel `wacom` driver serves
  and xf86-input-wacom.

### Routes considered

- **Private seat with static devices (Decisions 2–4):** works, but only for
  a desktop started after the tablet existed, so it needs a sign-out.
  Rejected.
- **`Xorg -seat` per desktop with the tablet on that seat:** a non-`seat0`
  server silently skips foreign devices, and GDM's log would not even
  mention the tablet. But the lab's NVIDIA server failed with "No devices
  detected", and the session's own uinput devices would have to move to that
  seat too. Not pursued now.
- **Starting Xorg with the device present:** needs the tablet before the
  desktop, which the protocol cannot give without an authentication-time
  device field.
- **Tag-based hotplug (adopted).**

## Amendment (2026-11-11): pen contact on a bridged tablet (`PressureRecalibration off`)

### Facts

- xf86-input-wacom (1.0.0 on EL9, `src/wcmCommon.c` `rebasePressure`,
  `normalizePressure`, `setPressureButton`) by default takes the first
  pressure a pen reports in proximity as that pen's zero ("worn pen"
  recalibration), and presses button 1 only when the rebased pressure
  passes the threshold. Its manual (`wacom(4)`, `PressureRecalibration`)
  names the downside: a pen whose first reading is already in contact
  produces pressure near zero and no button event unless the pressure later
  rises past the landing value.
- A tablet on a desk is read every few milliseconds, so the first sample in
  proximity is a hover at zero. A bridged tablet is read once per round trip:
  Linux `usbhid` keeps one interrupt transfer in flight, and each completion
  crosses the network (lab: one pen report every 41 ms at a 32 ms Deck-Pier
  round trip). A pen put down at an ordinary speed then often arrives
  already in contact.
- Lab (Rocky 9.7, Firefox 140 ESR and Chrome 155, a uinput twin of the bridged Intuos Pro
  L pen node replaying 36 ms samples into the desktop through the same
  `arcen_bridge_N` class): a stroke that hovers first draws one stroke with
  pressure about 0.47; the same stroke landing in contact reports pressure
  0.01-0.04 and breaks into two or three short `pointerdown`/`pointerup`
  segments. A drawing page that scales line width and opacity by pressure
  (`hardwaretest.org/stylus-test`: width `max(0.5, 20·p)`, opacity
  `max(0.2, p)`) shows nothing visible, while its pressure readout still
  moves. With `PressureRecalibration` off, the landing stroke is one stroke
  at pressure about 0.44.

### Decision

The desktop's own bridge class sets `Option "PressureRecalibration" "off"`
(`arcen_usb_bridge::bridge_seat::XORG_BRIDGE_PRESSURE_RECALIBRATION`). The
host's own driver keeps the device, as Native tablet promises; only this
default, which assumes a locally sampled tablet, changes.

### Consequences

- A worn pen whose tip never reads zero now clicks when it enters proximity
  instead of being rebased. The driver still logs its "worn out pen"
  warning; replacing the nib fixes it, as on a local desktop.
- A desktop started by an earlier Pier ends with the previous class and is
  refused as `DesktopPredatesBridgeClass` until its user signs out.
- Routes considered: switching bridged tablets to xf86-input-libinput (no
  recalibration, but libinput ignores bridged nodes by design here and the
  0.17 libinput pen was Arcen's own virtual device, not the host's driver);
  raising or lowering `Threshold` (does not help: the rebased pressure stays
  near zero); queueing every physical report on the Deck so the host sees
  native-rate samples (latency grows without bound when 200 reports per
  second meet one transfer per round trip). How the Deck should pace a
  physical interrupt endpoint over a WAN, including one-shot proximity
  reports, is a follow-up.

## Amendment (2026-11-12): native-seat Wayland, pen contact and the Deck's cursor

### Facts

- A native-seat Wayland desktop takes the bridged tablet through libinput
  (1.30.1 on Rocky 10.2), not xf86-input-wacom. libinput detects a pressure
  offset on a proximity-in only when the tool hovers at least half the
  distance range above the surface (`evdev-tablet.c`,
  `detect_pressure_offset`); a pen whose first packet already presses reports
  distance 0, so its stroke keeps its pressure. Lab (Rocky 10.2, Mutter 49.4,
  production Pier with `usb.native_tablet` enabled, emulated Intuos5 over
  `vhci-hcd`, `--usb-emulate-stroke alternate`): libinput reports
  `proximity-in` with pressure 0.50 and `TIP down` in the same frame;
  Firefox 140 ESR and Chrome 155 (both Wayland-native) draw one stroke with
  `pointerType` `pen` from the first packet. Nothing on the Wayland path
  corresponds to `PressureRecalibration`.
- Mutter's native backend keeps each tablet tool's position apart from the
  core pointer (`meta-seat-impl.c`, `update_device_coords_in_impl`), and a
  `ScreenCast` stream's cursor metadata reports only the core pointer
  (`meta_cursor_tracker_get_pointer` → `clutter_seat_query_state` with no
  device). Lab: zero `CursorPosition` records reached the Deck during four
  whole emulated strokes, although `cursor_positions` was negotiated.

### Decision

On a native-seat desktop whose Deck draws its cursor at host positions, the
Pier reads the bridged pen's own evdev node (read-only, never grabbed; the
compositor keeps the device) for as long as the attachment lasts and reports
where GNOME's default mapping puts the tool: the whole tablet area on the
whole output, aspect not kept (`arcen_input::tablet_pointer`). When the tool
leaves proximity the core pointer's last position is reported again
(superseded 2026-11-12 by [ADR 0023](0023-one-native-tablet-pen-position-path.md):
nothing is reported when the tool leaves).

### Consequences

- Lab: 103 `CursorPosition` records per four strokes, at exactly the pixels
  where the browser saw the pen (start `(442, 333)`, end `(1328, 778)` on a
  1800×1130 output).
- A tablet remapped in GNOME Settings (area, aspect, output, relative mode)
  still draws where GNOME puts it, but the Deck's cursor follows the default
  mapping. Reading the user's tablet settings is a follow-up.
- A multi-output desktop reports no host positions yet (as before).
- Routes considered: the core pointer (does not follow the tool in Mutter);
  `embedded` cursor mode (the Deck asked for a local cursor, and a cursor in
  the video is drawn twice); asking the compositor over D-Bus (Mutter
  exposes no tablet tool position).
