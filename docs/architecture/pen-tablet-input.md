# Typed pen/tablet input (local termination)

**Vendor neutrality (0.16.0).** Pen and tablet support is not tied to any
tablet maker. Every decision about *whether* a device is a pen tablet is made
from its HID report descriptor by shared code
(`arcen_usb_bridge::classify_pen_tablet`, see
[Pen-tablet classification](#pen-tablet-classification-vendor-neutral)), and
every pressure, tilt and rotation conversion goes through one shared,
vendor-neutral contract (`arcen_input::pen_axes`, see
[Axis normalisation](#axis-normalisation-vendor-neutral)). Vendor ids are
logged for readability and never used as a gate. The validation history
below names the tablet that was physically tested; that is a record of what
was measured, not a list of what is supported.

**Status (2026-08-03):** Wire contract, macOS capture, Linux and Windows
injection backends, and the raw-HID quarantine described below are
implemented and unit/portable-tested — see
[`legal/ORIGINS.md`](../../legal/ORIGINS.md) 2026-07-25
and 2026-07-28 intakes. **Live human hardware validation completed 2026-08-03:
a Wacom Intuos5 touch L pen was physically moved, pressure-applied, tilted,
and clicked through Deck's Tablet Monitor, with 1,572+ forwarded pen events
(including pressure 0.0–1.0 and tilt) reaching both a Linux (pier-linux.example.internal, Xorg
uinput path) and Windows (pier-windows.example.internal, PT_PEN path) host. See
[capability coverage](#capability-coverage-and-improvement-boundaries) for
the confirmed supported subset, known limits, and what requires USB
redirect.**

### macOS AppKit event gate — root cause and fix (2026-08-03)

During validation a critical capture bug was found and fixed: tablet drivers
on macOS (measured with a Wacom) deliver pen motion as **mouse/drag events
with a `TabletPoint` subtype**,
not as standalone `NSTabletPoint` events. For these events `vendorID()` and
`pointingDeviceType()` both return **0**. The original gate in
`clients/macos/src/tablet/monitor.rs` dropped every event where `vendor_id =
0`, so only proximity enter/leave reached the host — all position, pressure,
tilt, and button events were silently discarded.

Fix (`clients/macos` commit `1d6af87`): the gate now blocks only
`vendor_id = 0x05AC` (Apple — the sole source of false-positive
tablet-shaped events from trackpads) and promotes everything else that passed
the `effective_kind` gate to `Pen`. The old vendor-allowlist approach was
inverted: rather than requiring a known vendor, it now requires only the
absence of the known-bad vendor. Since 0.16.0 the test-only vendor list that
remained in `monitor.rs` is gone too.

## Problem and scope

Arcen Deck previously shipped a partial raw-HID-over-IP path (`clients/macos/src/hid/`,
Linux `/dev/uhid` admission) alongside documentation implying full Wacom
passthrough. The implementation could not substantiate that claim: it had no
host-to-client control/feature/output/USB transfer channel, forwarded every
Wacom interface (including the plain USB mouse interface) by vendor ID alone,
and accepted client-supplied HID descriptors at the host's kernel-facing
parser boundary. That path is now quarantined (see
["Experimental raw-HID passthrough (quarantined)"](#experimental-raw-hid-passthrough-quarantined)
below) and is not part of default builds.

**Local termination** is the shipped feature this document describes: the
tablet driver already installed on the macOS client (any make) decodes the
physical tablet, AppKit delivers typed pressure/tilt/rotation/tool/proximity events to
Deck, Deck maps and forwards one small typed sample over the negotiated input
protocol, and each active Pier injects a native virtual pen device that any
ordinary Linux or Windows creative application already understands. Neither
host runs, needs, or requires a tablet vendor's driver.

**True USB bridging** — a future feature where a host owns the physical
device and full bidirectional USB semantics traverse the network — is a
different feature, driver, latency, and security boundary. It is **not**
implemented, is **not** what the raw-HID quarantine or its future removal
enables, and is described only as a scoped-out future decision boundary
below (["Future true USB bridge (not implemented)"](#future-true-usb-bridge-not-implemented)).

## Data path

```text
Any pen tablet + its macOS driver
  -> AppKit NSEvent tabletPoint / tabletProximity
  -> clients/macos/src/tablet/monitor.rs   (main-thread AppKit local event monitor, RAII)
  -> clients/macos/src/ui/pen_input.rs     (sends each sample as AppKit dispatches it)
  -> clients/macos/src/tablet/mapper.rs    (pure native-sample -> arcen_input::PenEvent mapper)
  -> arcen_input::PenForwardingDispatcher  (shared edge classification; every sample forwarded)
  -> input/cursor lane, arcen_input::InputLaneOutbox (back-pressure: hover folds, ink kept)
  -> arcen_protocol::messages::PenEventMsg (negotiated input protocol v3; PenEventMsg::validate())
  -> Linux hosts/linux/src/input/uinput.rs "Arcen Virtual Tablet Pen" uinput device
     OR Windows hosts/windows/src/input.rs PenInjector (synthetic PT_PEN pointer device)
  -> remote creative application (Krita, GIMP, Photoshop, ...)
```

No vendor SDK, vendor report parsing, or proprietary dependency is used
anywhere in this path. AppKit — not raw HID report decoding — is the capture
boundary; the tablet's own driver remains responsible for device-specific
decoding and pressure curves (AppKit hands Arcen pressure already scaled to
`0..1`, whatever the pen's native resolution), while Arcen owns a small,
stable, vendor-neutral pen contract (`arcen_input::PenEvent`,
`arcen_protocol::messages::PenEventMsg`).

## Pen-tablet classification (vendor-neutral)

Module: `shared/usb-bridge/src/hid_digitizer.rs` (pure, bounded, no OS calls;
tests with hand-written descriptors in `hid_digitizer_tests.rs`).

`parse_report_descriptor` walks a HID report descriptor (short and long
items, global Push/Pop, usage ranges, 32-bit extended usages, signed/unsigned
extents) into one summary per Application collection — top-level ones and
any nested inside them, each a device function of its own, with inputs
attributed to the innermost Application collection as a host does — refusing
descriptors over 4096 bytes, deeper than 32 collections, with more than 64
Application collections or 256 usages on one item, or that are truncated or
unbalanced. An opaque `0xFF0D` Digitizer/Pen application that wraps a
nested non-digitizer Application collection (say, a keyboard) is a wrapper,
not a pen, and does not classify as one.

`classify_pen_tablet` decides from all of a device's HID interfaces:

- **Pen tablet** when a standard Digitizers-page (`0x0D`) collection whose
  usage is Pen (`0x02`) or Digitizer (`0x01`), or that nests a Stylus
  (`0x20`), reports X and Y plus at least one of Tip Switch (`0x42`), Tip
  Pressure (`0x30`) or In Range (`0x32`). Touch-shaped collections (Finger,
  Contact Identifier, Touch Screen/Pad without a stylus) are not pens.
- **Vendor-defined Digitizers page** (`0xFF0D`, whose usage ids mirror the
  Digitizers page — the Linux HID stack treats it the same way). A top-level
  application collection there whose usage is Digitizer (`0x01`) or Pen
  (`0x02`) is a pen tablet even when its reports are opaque vendor bytes
  (`PenTabletEvidence::VendorDigitizerPen`), provided the device publishes no
  Generic Desktop Joystick, Gamepad or Multi-axis Controller collection
  (`NotPenTablet::DisqualifyingCollection`). Keyboard and keypad collections
  (express keys) are allowed, exactly as beside a standard pen. Its
  capabilities are only what it declares: an opaque collection reports none,
  and no pressure range is invented. This is how an Intuos5-style tablet
  (`0xFF0D` pen, `0xFF00` vendor control, Generic Desktop mouse) qualifies.
  Any other `0xFF0D` collection counts only beside a standard Digitizers
  collection; other vendor pages (a lone `0xFF00`) never qualify.
- **Refused** (fail closed): keyboards, mice (relative or absolute), gamepads,
  vendor-only devices, touch-only digitizers (including a keyboard beside a
  touch pad), digitizers without position or contact, and any device with a
  malformed descriptor. A keyboard or mouse is accepted only as part of a
  device that has a pen digitizer.

It also reports the declared surface (`PenCapabilities`): pressure range and
level count, X/Y tilt ranges with physical units, twist, eraser/invert, and the
number of barrel switches (Barrel `0x44`, Secondary Barrel `0x5A`).

Where it is used:

| Where | What it decides |
| --- | --- |
| Deck `tablet::presence` | whether a pen tablet is present, from every HID device's `ReportDescriptor` in the I/O Registry (no open, no permission), plus "AppKit already delivered a pen sample this run". Informational only (Tablet Monitor, diagnostics): typed pen is offered whenever capture is possible (`tablet::typed_pen_capability`), because the hello cannot be renegotiated when a tablet appears later. |
| Deck raw-HID capture (`hid/session.rs`) | match by digitizer usage page; offer an interface only if its descriptor declares a digitizer; forward reports only from pen-declaring interfaces. |
| `arcen-usb-helper` (root, macOS) | Hard USB candidate: every interface HID, and the same shared `authorize_hard_usb_tablet` the host applies (pen tablet, no game-controller collection, aggregate size). Descriptors come from the I/O Registry (`IOHIDDevice` `ReportDescriptor`, matched by vendor/product id, a known and equal `LocationID` when two identical devices are attached, and the interface's `bInterfaceNumber`; conflicting entries refuse the device), so no device is opened to be examined. The handshake size is checked before the device is seized. `GET_DESCRIPTOR(Report)` is sent only for an interface the registry left undescribed, on a device the registry already shows to be a digitizer and that has no boot-keyboard interface. |
| Linux Pier `usb_bridge::authorize_device` | before creating the `usb-vhci` device: `authorize_hard_usb_tablet` over the Deck's claimed descriptors; afterwards `BridgedDeviceGuard` checks every device, configuration and report descriptor the device returns before giveback. |
| Linux Pier raw HID (`net/server.rs`) | `RawHidAdmission`: a `/dev/uhid` device only for a pen-bearing interface of a device that classifies as a pen tablet, and never for a descriptor that also carries a joystick, gamepad or multi-axis controller collection. Express-key keyboard collections in the pen's descriptor are admitted with it. |

### Security: what bridging forwards, and why

Hard USB forwards every collection of every interface of the device, and raw
HID every collection of an admitted descriptor. That is the point: a whole
tablet — pen, touch strip, mouse mode and express keys — reaches the host's
own driver.

**Express keys are forwarded.** Many tablets (Huion, XP-Pen, Gaomon and some
Wacom models) present their express keys and buttons as a Generic Desktop
Keyboard or Keypad collection, often as a boot-keyboard interface. A device
that classifies as a pen tablet may carry those, and they are bridged with it.
This grants nothing new: the authenticated Deck can already send any
keystroke to the host session through the ordinary input channel, so
forwarding a composite tablet's keyboard is not an escalation. What does
matter, and stays enforced:

- **It must be a pen tablet.** The device must classify as one
  (`classify_pen_tablet`): a plain keyboard, or a keyboard beside a
  touch-pad-only digitizer, is never forwarded.
- **No non-tablet functions.** `authorize_hard_usb_tablet` refuses a device
  if any interface's descriptor carries a Generic Desktop Joystick, Gamepad
  or Multi-axis Controller collection at any depth — top-level, nested as an
  Application collection, or named by a physical/logical collection
  (`ForwardsNonPenInput`); raw HID refuses such a descriptor
  (`RawHidRefusal::NonPenCollection`).
- **Bounded claims.** The descriptors together may not exceed
  `MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES` (12 KiB, which always fits the helper
  handshake frame).
- **HID only, bound to the claims.** `BridgedDeviceGuard` (`shared/usb-bridge/src/bridged_device_guard.rs`)
  binds the device's own answers to the authorization: device and
  device-qualifier descriptors must keep class `0x00`/`0x03`, the authorized
  vendor/product/revision and one configuration; configuration and
  other-speed configuration descriptors must parse within bounds with
  interrupt endpoints only, every interface and alternate setting HID (a
  mass-storage or any other non-HID interface ends it), and exactly the
  authorized interface numbers (only the 9-byte header may be read
  partially), and each interface's HID class descriptor must be well formed
  (`bLength` exactly `6 + 3 × bNumDescriptors`, at most 16 entries) with
  exactly one Report entry — hosts disagree on which of several wins, Linux
  taking the last — advertising exactly the authorized report-descriptor
  length, so the host never reads a
  shorter prefix that hides the pen; a report-descriptor completion must be
  exactly `min(wLength, authorized length)` bytes of the classified
  descriptor, so a prefix is accepted only when the host asked for fewer
  bytes. A violation ends the attachment
  (`usb_hard_device_refused` with the reason) before the host kernel sees
  the descriptor.

## Axis normalisation (vendor-neutral)

Module: `shared/input/src/pen_axes.rs`. The wire contract is pressure
`0.0..=1.0`, tilt `-90.0..=90.0` degrees, rotation `0.0..360.0` degrees. Every
conversion into or out of it is shared:

- Deck (`tablet/mapper.rs`): `clamp_pressure`, `tilt_unit_to_degrees` (AppKit
  normalized tilt × 90), `wrap_rotation_degrees`. Non-finite input still
  reaches `PenEvent::validate` and is rejected, never invented.
- Device ranges: `normalize_logical`/`scale_unit` over a `LogicalRange`
  (tested with 1023/2047/4095/8191/16383 maxima: the same physical force lands
  on the same host value within one host step), `tilt_logical_to_degrees`
  (physical degree extent when declared, else logical extremes are full
  deflection), `twist_logical_to_degrees`.
- Linux (`hosts/linux/src/input/pen.rs`): `ABS_PRESSURE` is
  `TYPED_PEN_PRESSURE_RANGE` (`0..=65535`, 16 bits — finer than any shipping
  pen, and no vendor's maximum); tilt via `whole_tilt_degrees`.
- Windows (`hosts/windows/src/input.rs`): pressure via
  `WINDOWS_INK_PRESSURE_RANGE` (`0..=1024`, the Windows Ink API ceiling),
  tilt/rotation via `whole_tilt_degrees`/`whole_rotation_degrees`.
- macOS Pier (`hosts/macos/src/input/pen.rs`): `clamp_pressure`,
  `tilt_degrees_to_unit`, `wrap_rotation_degrees`. Its proximity events report
  vendor id `0xFFFF` (reserved, never assigned; Arcen's id for synthetic
  devices), not a tablet maker's.

In Native tablet (USB bridged) mode no conversion happens at all: the host
kernel reads the device's own report descriptor, so the pen's native pressure
resolution and every axis it declares reach the host driver unchanged.

## Wire negotiation (protocol v3, input v3)

Full field-level contract: [`shared/protocol/WIRE.md`](../../shared/protocol/WIRE.md)
("Negotiated typed pen (protocol v3, input v3)"); semantic contract:
[`shared/input/ARCHITECTURE.md`](../../shared/input/ARCHITECTURE.md).

- `input_protocol_version` moved from 2 to 3 (`wire::PROTOCOL_VERSION` stays
  3 — purely additive). Both hellos' `InputCapabilitiesMsg` gained six
  `#[serde(default)]` pen fields — `pen`, `pen_pressure`, `pen_tilt`,
  `pen_rotation`, `pen_eraser`, `pen_proximity` — each an
  `InputCapabilityAvailability` (`available` \| `unavailable` \| `unknown`,
  default `unknown`).
- **`unknown` never authorizes typed pen.** A product may activate local
  termination only when **both** peers advertise `input_protocol_version >= 3`
  **and** `pen = available`, plus whichever specific axis capability
  (`pen_pressure`/`pen_tilt`/`pen_rotation`/`pen_eraser`/`pen_proximity`) it
  depends on. An input-v2-or-older peer never advertises these fields, is
  never authorized to send or receive `PenEventMsg`, and keeps ordinary
  mouse/keyboard behavior unchanged — this is the automatic-activation
  precondition referenced throughout this document: **automatic local
  termination requires input v3 plus both peers' pen capability plus the
  client's own `tablet_input_enabled` setting; it is not automatic on older
  peers or with the setting disabled.**
- `PenToolMsg` (`tip` \| `eraser`) and `PenEventMsg`/`PEN_EVENT`
  (`"pen_event"`) carry normalized `x`/`y`, `-1`-sentinel `server_x`/`server_y`,
  `pressure`, `tilt_x_degrees`/`tilt_y_degrees`, `rotation_degrees`, `tool`,
  `in_proximity`/`touching`, a `buttons` bitset (bit 0 = first barrel button,
  bit 1 = second), and the shared `sequence`/`timestamp_ns`/`coalescable`
  metadata used by every other input message.
- **Validation is `PenEventMsg::validate()`, not a custom `Deserialize`.**
  `serde` alone accepts an untrusted peer's out-of-range finite value (e.g.
  `pressure: 2.0`); every product boundary calls `validate()` — checked
  ranges `x`/`y` `0.0..=1.0`, `pressure` `0.0..=1.0`, `tilt_x_degrees`/
  `tilt_y_degrees` `-90.0..=90.0`, `rotation_degrees` `0.0..=360.0` (both
  bounds are the same physical angle) — and rejects a failing message before
  conversion, sequence advancement, or injection.
- `PenEventMsg`/`PenToolMsg` live in `arcen-protocol`; the canonical semantic
  `arcen_input::PenEvent`/`PenTool` live in `arcen-input`. `arcen-input`
  depends on `arcen-protocol` only so the region-input encode/decode pair
  (`RegionInputWireMessage`) can be shared; `arcen-protocol` never depends on
  `arcen-input`. Outside the `Region*` family each product still performs the
  checked wire-to-semantic conversion at its own boundary.
- `sequence` participates in the single globally ordered input stream shared
  with keyboard, absolute/relative motion, and buttons/wheel; a rejected or
  out-of-order sequence never advances product state, and coalescing may only
  drop superseded hover/move samples — proximity, contact, tool, and button
  edges are never dropped.

## Tablet mode contract (requested vs active)

Deck now carries an explicit per-connection tablet mode request in
`ClientHelloMsg.tablet_mode_requested` and both peers advertise
`tablet_mode_capabilities`. The host answers with `tablet_mode_result`, so the
client can show the effective mode and reason.

- **Tablet support** (`local_termination`) remains the default and the only
  WAN-recommended mode. It becomes active when the input-v3 pen path is
  available and the user left typed pen enabled; the Deck offers it whether
  or not a tablet is attached at connect time, because AppKit tablet events
  are the source and the hello is not renegotiated when a tablet appears.
- **Native tablet mode** (`TabletModeMsg::UsbTabletBridge`, wire token
  `wacom_usb_bridge` kept for 0.15 compatibility — a legacy name, the mode is
  vendor-neutral) is an explicit LAN/KVM choice and must not be conflated with
  generic HID/PT_PEN injection; bridge parity requires complete USB semantics
  plus the host's own driver for that tablet. It is offered for any device
  that classifies as a pen tablet. If unavailable,
  the host rejects it and activates `disabled_mouse_compat`, never
  `local_termination`.
- **Mouse compatibility only** (`disabled_mouse_compat`) is the fail-safe and
  user-selected compatibility mode.

## macOS capture and Deck integration

Module: `clients/macos/src/tablet/` (`mod.rs`, `sample.rs`, `probe.rs`,
`monitor.rs`, `mapper.rs`, `runtime.rs`) and `clients/macos/src/ui/pen_input.rs`;
wired into
`clients/macos/src/ui/app.rs`, `clients/macos/src/ui/macos_menu.rs`, and the
pen-capability parts of `clients/macos/src/transport/websocket.rs`.

- `monitor.rs` is a main-thread-owned AppKit local `NSEvent` monitor RAII
  guard (`objc2`/`objc2-app-kit`/`block2`; no vendor SDK) that hands each
  `tabletPoint`/`tabletProximity` sample, with how long it waited for
  dispatch (`NSEvent.timestamp`), straight to `ui/pen_input.rs`. Only an event
  pen input cannot take at that moment (its state is borrowed, or an earlier
  event is still queued) goes to a bounded producer queue the next UI pass
  drains, in order.
  `runtime::TabletRuntime::install()` only ever succeeds on macOS, on the
  main thread; it degrades to an inert `None` everywhere else (off-macOS
  build, off-main-thread call, or AppKit refusing to install) and callers
  treat that as "no typed capture this run," not an error — the
  mouse-emulation fallback stays fully functional either way.
- `mapper.rs` (`TabletMapper`) is a pure, unit-tested function from one
  native sample plus the current `ViewSize`/`image_rect` mapping to
  `arcen_input::PenEvent`. `NSEvent.tilt()` reports normalized `-1.0..=1.0`
  axis components; the mapper's `tilt_to_degrees()` (`tilt * 90.0`, clamped)
  is original Arcen work matching the W3C PointerEvent `-90..=90` degree
  convention. `NSEvent.rotation()` is wrapped into `0.0..360.0` because Apple
  does not guarantee one sign convention across tablet drivers. Window
  points are mapped through Deck's current video `image_rect` and selected
  monitor, accounting for AppKit's bottom-left origin, backing scale,
  letterboxing, and fullscreen transitions.
- **Input never waits for the UI or presentation loop.** `ui/pen_input.rs`
  (`PenInput`) maps, classifies and sends a sample in the same AppKit
  callback that delivered it, stamped with the event's own time. The UI pass
  only publishes routing (video `image_rect`, multi-window targets, region
  viewports) and session state. Measured before this: a Deck on a 30 Hz panel
  forwarded pen samples every 33 ms, because AppKit coalesced the digitizer's
  reports while the main thread waited for vsync and samples left only on the
  next UI pass. Now:
  - AppKit mouse-event coalescing (`NSEvent.isMouseCoalescingEnabled`, which
    covers tablet events) is turned off while a session window is active and
    restored on deactivate and quit, so every digitizer report arrives;
  - the UI never asks for passes faster than the root display refreshes, and
    pen motion no longer wakes the UI (only edges do), so the main thread is
    free to dispatch events;
  - keys, modifiers, scrolling and mouse buttons still go through the UI pass,
    so the monitor marks them, and pen samples dispatched after one wait for
    that pass (bounded, fail-closed) to keep the person's order;
  - the session telemetry reports `input_dispatch_age_p50/p99/max_us`,
    `pen_forwarded`, `pen_forward_interval_p50_us` and
    `pen_held_for_ordering`.
- `arcen_input::PenForwardingDispatcher` (shared, `shared/input`) classifies
  edges: proximity, contact, tool and button transitions are marked not
  coalescable and never dropped; motion is marked coalescable but still
  forwarded, because pressure samples with the tip down are the ink.
  `release_event()` emits one final synthetic out-of-proximity release so a
  remote tool can never be left logically stuck down. Only real lane
  back-pressure folds pen motion (`InputLaneOutbox`): hovering samples fold
  into the previous hover of the same stroke state; tip-down samples are kept
  until more than `PEN_CONTACT_BACKLOG_CAP` (256) wait, and every fold beyond
  that is counted.
- `presence.rs`/`hid_inventory.rs` answer **tablet presence** without a
  vendor id: `tablet_presence()` is `Available` when any HID device in the
  I/O Registry publishes a report descriptor that declares a pen or a
  vendor-mirrored digitizer (USB or Bluetooth; read-only, no Input
  Monitoring), or when AppKit already delivered a pen/eraser sample this run;
  `Unavailable` when none does; `Unknown` if the registry cannot be read.
  Presence is informational. The hello's `pen`/`local_termination` follow
  `typed_pen_capability(tablet_input_enabled)` instead: `Available` on the
  macOS Deck whenever the setting is on, so a tablet connected late or not
  described by the registry still gets pressure without a reconnect. A 0.15
  host resolves this exactly as it did for a Deck with a tablet attached.
- `probe.rs` adds **empirically observed axis capability**
  (`TabletCapabilityProbe`: pressure/tilt/rotation/eraser/barrel-buttons each
  start `Unknown` and move to `Available` only once a delivered sample
  actually demonstrates that axis in use — never synthesized, never inferred
  from silence).
- `tablet_input_enabled` (default `true`, the Tablet support mode in
  Settings) gates client pen-capability
  advertisement; see [`clients/macos/SETTINGS.md`](../../clients/macos/SETTINGS.md).
  A settings file predating this key defaults it to `true` on load.
  Disabling it, losing window focus, leaving proximity, or session
  teardown/reconnect all restore ordinary mouse handling, releasing any
  mid-contact pen state with one final synthetic release. Mouse-emulation
  duplicates are suppressed only while a real pen/eraser holds authority.
- Local termination activates only when the negotiation in the previous
  section succeeds (input v3, both peers' pen capability, and this setting
  enabled); otherwise Deck preserves ordinary mouse behavior and shows an
  honest pressure-unavailable downgrade — it never fabricates pressure.
- **View > Tablet Monitor** (`ui/app.rs::paint_tablet_monitor_panel`) is the
  local-only diagnostic panel (never logged or persisted): pen-tablet
  presence, whether the runtime installed, negotiated state, current
  tool/proximity/touching, live pressure/tilt/rotation, lifetime
  events-sent/overflow-reset counters, producer-drop and dispatcher
  edge/motion-forwarded/overflow counters, the empirical capability probe, and the
  last error string. It reports device/capability/liveness facts only —
  never coordinates, pressure samples, raw report bytes, or other input
  content.
- **Express keys and touch rings/strips are configured in the tablet
  vendor's control panel on the client**, not by Arcen, to emit ordinary keyboard or wheel input,
  which Deck already transports through its existing keyboard/mouse-wheel
  path. Arcen ships no ExpressKeys/ring-specific protocol or mapping UI.
- **Tablet multi-touch is not transported.** Only stylus/eraser samples
  travel through `PenEventMsg`; this remains an explicit, documented
  exclusion, not a gap to silently work around.
- Only one active tablet/tool per Deck session is supported.

## Linux backend (uinput tablet-tool device)

Module: `hosts/linux/src/input/pen.rs` (portable, `evdev`-free pure mapping
and idempotent tool/button state machine — kept dependency-free so it is
unit-testable off Linux) and `hosts/linux/src/input/uinput.rs`
(`build_tablet_device`, `InputController::pen_event`/`pen_available`).

- A **separate** virtual device, `"Arcen Virtual Tablet Pen"`, distinct from
  the existing absolute/relative mouse `uinput` device: `libinput`/Xorg
  classify a device by its complete capability set, and merging tablet
  `ABS_PRESSURE`/`BTN_TOOL_PEN` onto the plain pointer device would make the
  ordinary mouse device misclassify as a tablet everywhere.
- Advertises `ABS_X`/`ABS_Y` (same fixed device raster as the mouse device),
  `ABS_PRESSURE` (inclusive `0..=65535`, `pen::PRESSURE_MAX`, the shared
  vendor-neutral `arcen_input::pen_axes::TYPED_PEN_PRESSURE_RANGE`: the wire
  carries normalized pressure, so the virtual device adopts no tablet maker's
  range, and 16 bits is finer than any shipping pen; libinput normalizes by
  the declared extent), `ABS_TILT_X`/`ABS_TILT_Y` (whole-degree `-90..=90`,
  passed straight through from the wire's degree convention, with a
  resolution of `arcen_input::virtual_device::TILT_UNITS_PER_RADIAN` (57
  units per radian) — libinput reads a tilt axis without a resolution as a
  Wacom pen's ±64° spread over the range, which delivered a 45° tilt as 32°
  to Chrome on Xorg and to every Wayland client. On Xorg this is a deliberate
  trade-off: xf86-input-libinput exposes tilt as a ±64 valuator, as for a USB
  Wacom, and Firefox reads it through GDK as ±1 and multiplies by 90, so it
  reports tilt 90/64 larger than the degrees Chrome reports), tool/proximity
  via `BTN_TOOL_PEN`/`BTN_TOOL_RUBBER`, tip contact via `BTN_TOUCH`, and two
  barrel buttons via `BTN_STYLUS`/`BTN_STYLUS2`.
- **The tablet device is the pen; the mouse device is not used for it.**
  xf86-input-libinput puts the tablet device on the master keyboard and gives
  each tool in proximity its own slave pointer (`"… Pen (0)"` in
  `xinput list`), which moves the cursor and presses button 1 for the tip and
  buttons 2 and 3 for the lower and upper barrel switch, as with a USB tablet.
  Until 0.17.0 the pen also moved the cursor and clicked through the
  absolute mouse device, so X clients received each sample twice from two
  source devices: Firefox began every stroke with a `mouse` pointerdown and
  switched pointer type and id inside strokes, and pages that end a stroke on
  `pointerleave` lost ink. Without a tablet device (creation failed, or Hard
  USB released it) the pen falls back to cursor and clicks on the absolute
  device, without pressure or tilt, with the same buttons (tip = left, lower
  barrel = middle, upper barrel = right).
- **Contact comes from pressure.** libinput ignores `BTN_TOUCH` on a tablet
  with a pressure axis and puts the tool in contact when pressure reaches 5 %
  of the range, out of it at 1 % (`evdev-tablet.c`, libinput 1.19 to 1.31). A
  Deck contact can carry little or no pressure (a tap, the start of a light
  stroke, a pen without pressure: `touching` is the tip or any pressure), so
  `pen::PenAxes` reports `0` out of contact and at least the device's contact
  threshold in contact (`arcen_input::virtual_device::contact_pressure`). The
  Xorg tablet's quirk sets the thresholds to `AttrPressureRange=2:1`
  (`virtual_device::PRESSURE_CONTACT_UPPER`/`LOWER`), so the floor is 2 of
  65535 there; the Wayland lab seat pen is read by the compositor's libinput
  with the system quirks only, so its floor is libinput's default threshold
  plus one (3278, `libinput_default_contact_pressure`): its lightest contact
  reads about 4 %. Without readable system quirks the session keeps
  libinput's defaults (the launcher warns), and Xorg contacts lighter than
  5 % are lost.
- **Rotation is deliberately `Unavailable` on Linux.** `build_tablet_device`
  advertises no rotation axis, and `hosts/linux/src/session/handshake.rs`
  always sets `pen_rotation = Unavailable` regardless of whether the tablet
  device was created, because no target here has proven the kernel/libinput
  stack recognizes a chosen evdev axis as tablet rotation — an honest
  `Unavailable` is preferred over an unproven claim.
- `plan_pen_edges()` emits ordered, idempotent `EV_KEY` edges: entering
  proximity asserts the tool bit before touch/button edges; leaving
  proximity releases touch/buttons before the tool bit (matching physical
  tablet ordering); an out-of-proximity sample is always treated as fully
  released regardless of what a stale/malformed peer still carries in
  `touching`/`buttons`, so a tip or barrel button can never be left
  logically stuck once the tool has physically lifted. Every held code is
  released on reset/drop (`reset_pen_held`).
- The pen device is probed/created **before** `ServerHello` is built
  (`InputController::pen_available`), so Linux advertises `pen`/
  `pen_pressure`/`pen_tilt`/`pen_eraser`/`pen_proximity` as `Available` only
  when the runtime backend actually established the device — never an
  aspirational default. If pen-device creation alone fails, mouse and
  keyboard remain available (see `hosts/linux/ARCHITECTURE.md` for the full
  fail-open-for-mouse contract).
- **No tablet vendor driver runs on the Linux host.** `libinput`/Xorg see an
  ordinary kernel tablet-tool device; the Linux target is the dedicated
  Arcen Xorg session described in `hosts/linux/ARCHITECTURE.md`.
- **libinput does not smooth the virtual pen.** libinput (1.11 and later)
  averages a tablet tool's last four positions and tilts unless a quirk sets
  `AttrTabletSmoothing=0` (`evdev-tablet.c`, `tablet_init_smoothing`); the
  mouse is never smoothed. The samples the Deck sends were already filtered
  by its own tablet driver, so the average only delayed the pen by about one
  and a half samples: measured on the real screen, pen motion reached a
  browser in the session 12-15 ms later than the same motion from the mouse.
  The session launcher starts its Xorg with `LIBINPUT_QUIRKS_DIR` pointing at
  a session-private copy of the system quirks plus
  `arcen_input::virtual_device::libinput_quirks()` and the administrator's
  `local-overrides.quirks` last (`prepare_libinput_quirks`); no system file
  changes. Without readable system quirks it keeps libinput's defaults.
- **libinput keeps the pen in proximity while it hovers still.** libinput
  forces a tablet tool out of proximity after 50 ms without events until the
  device has once reported a genuine release of the tool
  (`FORCED_PROXOUT_TIMEOUT`, `evdev-tablet.c`; libinput 1.19, 1.30, 1.31). A
  pen held still above the Deck's tablet sends nothing, so a jump to a new
  point arrived only as a fresh proximity-in that applications did not see as
  motion (measured on the Wayland lab seat pen: no host event for 16 pen
  jumps). The first time a tool enters proximity on a virtual tablet, the
  Pier injects one genuine pen-tool in/out before the real sample, even when
  the eraser approaches first (libinput disarms only on a `BTN_TOOL_PEN` release)
  (`arcen_input::virtual_device::ProximityTimeoutDisarm`; the uinput tablet
  here and the lab seat pen).
- Parse/validate every `PenEventMsg` through the existing globally sequenced
  input dispatcher before mapping or emitting.

## Linux Wayland sessions (emulated stylus)

A headless Wayland session cannot use the uinput tablet tool above: its
compositor opens no input devices, and the device would reach the
machine's physical seat. Pen goes only to an **emulated stylus** offered by
the compositor's EIS server: libei's `ei_stylus` interface, merged for
libei 1.7 (libinput/libei!400). The session agent probes for it at start.
The hello advertises each axis only from what the stylus bound
(`arcen_input::pen_emulation::PenCapabilityTruth`), so rotation can be
available there although the uinput tool never offers it. The new
`pen_barrel_buttons` capability says whether barrel buttons reach the
desktop.

`arcen_input::pen_emulation::StylusEmulator` turns samples into
protocol-valid frames on top of the shared `plan_pen_edges`. It is tested
exhaustively against a model of the protocol, and was driven through
libei's `eis-demo-server` without a protocol error. Today no GNOME release
offers an EIS stylus. A Wayland session therefore refuses Tablet support
with the evidence (`stylus_not_offered`, or `no_emulated_input` on
GNOME 40), and it refuses Native tablet (Hard USB), which would reach
`seat0`. See
[`linux-wayland-provider.md`](linux-wayland-provider.md#pen-and-hard-usb-in-a-wayland-session).

## Windows backend (synthetic PT_PEN pointer)

Module: `hosts/windows/src/input.rs` (`PenInjector`, `PenPointerState`,
`pen_pressure_to_windows`/`pen_rotation_to_windows`/`pen_tilt_to_windows`,
`pen_tool_flags`).

- An RAII `PenInjector` wraps one Windows 10 1809+ synthetic `PT_PEN`
  pointer device created with the public
  `CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)` /
  `InjectSyntheticPointerInput` / `DestroySyntheticPointerDevice` API — not
  `SendInput`, and not a Wintab driver. **Arcen makes no Wintab claim**;
  legacy Wintab-only creative applications are out of scope for this
  backend.
- Pressure maps to the **documented `0..=1024`** Windows Ink pointer-pressure
  range (`pen_pressure_to_windows`: the shared `scale_unit` over
  `WINDOWS_INK_PRESSURE_RANGE`) even
  when the source tablet reports more levels — this is a public Win32 API
  ceiling, not an Arcen-imposed limit, and this document does not claim 8192
  levels through that API.
- Rotation maps to `0..=359` (`pen_rotation_to_windows`), tilt to `-90..=90`
  (`pen_tilt_to_windows`), and eraser/barrel state maps to `PEN_FLAG_ERASER`/
  `PEN_FLAG_BARREL` in `POINTER_PEN_INFO` (`pen_tool_flags`: bit 0 of
  `PenEventMsg::buttons` is barrel/tip button truth; `tool == Eraser`
  selects the eraser flag). `PenPointerState`/`PenPointerEdge` is a small,
  unit-tested proximity/hover/contact transition state machine
  (`Out -> Hovering -> Contact` and back) driving the exact
  `POINTER_FLAG_INRANGE`/`_DOWN`/`_UPDATE`/`_UP`/`_INCONTACT` combination
  Microsoft documents for each transition.
- `pen_pixel_location()` reuses the same validated selected-output geometry
  already used for mouse `POINTER_INFO.ptPixelLocation`; pen is never routed
  through mouse `SendInput`.
- The handle and state live in the interactive user-session agent process,
  never the LocalSystem service, matching the existing Windows Host
  process-boundary rule (`hosts/windows/AGENTS.md`).
- Windows advertises `pen`/`pen_pressure`/`pen_tilt`/`pen_rotation`/
  `pen_eraser`/`pen_proximity` **together, all-or-nothing**
  (`hosts/windows/src/session.rs::build_server_hello`): the synthetic
  `PT_PEN` device either exists with every axis real from the same
  `POINTER_PEN_INFO` sample, or `CreateSyntheticPointerDevice` failed (older
  Windows, Windows Ink disabled, or a transient API failure) and every pen
  field advertises `Unavailable` — the attachment still proceeds mouse-only,
  never a silent partial/fake pen capability.
- Contact/proximity is released on disconnect, reconnect detach, focus
  reset, or injector drop.

## Capability coverage and improvement boundaries

Confirmed by physical validation with one tablet (a Wacom Intuos5 touch L,
2026-08-03). The Linux pressure range has since moved from 8191 to the
vendor-neutral 65535; no tablet of another make has been physically tested.

### Supported (end-to-end confirmed)

| Capability | Linux | Windows | Notes |
|---|---|---|---|
| Position X/Y | ✅ Full | ✅ Full | Normalised → screen pixels |
| Pressure | ✅ 65536 steps (16-bit) | ✅ 1024 levels (10-bit) | See ceiling note |
| Tilt X/Y | ✅ ±90° whole-degree | ✅ ±90° whole-degree | |
| Hover/proximity | ✅ `BTN_TOOL_PEN` | ✅ `POINTER_FLAG_INRANGE` | Enter/leave tracked |
| Tip contact | ✅ `BTN_TOUCH` | ✅ `POINTER_FLAG_INCONTACT` | |
| Eraser end | ✅ `BTN_TOOL_RUBBER` | ✅ `PEN_FLAG_ERASER` | |
| Lower barrel button | ✅ `BTN_STYLUS` → right-click | ✅ `PEN_FLAG_BARREL` | |
| Upper barrel button | ✅ `BTN_STYLUS2` → middle-click | ✅ `POINTER_FLAG_SECONDBUTTON` | |
| Rotation | ❌ Not injected | ✅ `PEN_MASK_ROTATION` | Protocol carries it; Linux omitted intentionally |

### Hard OS ceilings (not fixable in Arcen code)

**Windows pressure: 1024 levels.**
`InjectSyntheticPointerInput` documents `0..=1024` as the pressure range for
`PT_PEN` pointer data. A pen may report far more levels (8192 or 16384 are common); AppKit and the
protocol carry the full float normalised value; `pen_pressure_to_windows`
maps it faithfully to the 1024-step range. Closing this gap requires either
a kernel-mode Wintab driver (out of scope — USB redirect territory) or a
future Windows API change.

**Wintab-only applications on Windows.**
Some older creative applications (some legacy Photoshop/ArtRage builds) use
Wintab rather than Windows Ink (`WM_POINTER`/`GetPointerPenInfo`). The
`InjectSyntheticPointerInput` / `PT_PEN` backend is Windows Ink only. Wintab
support requires a kernel-mode HID driver, which is USB redirect territory.

### Improvable in Arcen code (without USB redirect)

**Linux barrel rotation.**
The protocol wire carries `rotation_degrees`; the uinput tablet device
deliberately omits a rotation axis because no confirmed kernel/libinput
combination has been validated to recognise it. The candidate axis is
`ABS_Z` (or `ABS_WHEEL`). If a target kernel/libinput/application stack is
confirmed to expose rotation from a uinput device, adding it is a small
one-axis change to `hosts/linux/src/input/uinput.rs` and the `pen_available`
handshake.

### Where to draw the line — USB redirect

The following capabilities **cannot be provided by local termination** and
require the future true USB bridge (see
["Future true USB bridge (not implemented)"](#future-true-usb-bridge-not-implemented)):

- **Full 8192 pressure levels on Windows** — requires a kernel-mode Wintab
  driver owning the physical device.
- **Wintab API support on Windows** — same requirement.
- **Tangential pressure / fingerwheel** (airbrush side-wheel) — a device
  feature not exposed by AppKit `NSEvent`; only accessible via raw HID.
- **Vendor-driver pressure curves and device-specific calibration** — the
  vendor's host driver must own the physical device; local termination
  intentionally lets the macOS driver run those curves before Arcen sees the
  event.
- **ExpressKey / touch-ring software mapping on the remote host** — buttons
  and touch-ring shortcuts already work as keyboard/scroll events through
  Deck's existing keyboard/wheel path (the macOS tablet driver handles the
  remapping); only host-side vendor control-panel configuration requires the driver
  present on the host.
- **4D Mouse / cursor tool** — `Cursor` tool type is explicitly rejected; USB
  redirect is the correct path.
- **Tablet multi-touch on the remote host** — stylus-path local termination
  intentionally excludes touch (see macOS capture section); multi-touch
  requires USB redirect or a dedicated touch protocol.

## Experimental raw-HID passthrough (quarantined)

Full wire detail: [`shared/protocol/WIRE.md`](../../shared/protocol/WIRE.md)
("Experimental raw-HID passthrough"). This is the pre-existing partial
IOHID/`/dev/uhid` path, not the typed-pen backend above, and it is **not**
the future USB bridge described below.

- **Default builds fully disable this path.** Admission requires **all** of:
  (1) the peer's own compile-time `experimental-raw-hid` Cargo feature
  (default off, in `clients/macos/Cargo.toml` and `hosts/linux/Cargo.toml`;
  Windows has no raw-HID receiver at all — `hosts/windows/src/session.rs`
  hard-codes `experimental_raw_hid: false`); (2) an explicit runtime opt-in
  (`ARCEN_EXPERIMENTAL_RAW_HID=1` in the process environment, checked by
  `crate::hid::experimental_raw_hid_client_opt_in()` on the client and
  `crate::input::experimental_raw_hid_runtime_enabled()` on the Linux host);
  and (3) both peers' independently negotiated `experimental_raw_hid` bool
  in `ClientHelloMsg`/`ServerHelloMsg` (`#[serde(default)] = false`,
  unrelated to and not gated by `input_protocol_version`/typed-pen
  negotiation).
  `clients/macos/src/transport/websocket.rs::should_start_experimental_raw_hid_capture`
  requires the client opt-in **and** the host's advertised capability
  together before starting any capture.
- `decode_hid_device_added`/`decode_hid_report` reject any claimed
  descriptor/report length above `MAX_HID_DESCRIPTOR_LEN`/
  `MAX_HID_REPORT_LEN` (4096 bytes each) unconditionally, independent of
  negotiation state, so a hostile or buggy peer can never hand an oversize
  payload to the host's kernel-facing `/dev/uhid` parser.
  `hosts/linux/src/net/server.rs` additionally bounds the admitted device
  count (`MAX_EXPERIMENTAL_RAW_HID_DEVICES = 8`) and admits a device only
  through `arcen_usb_bridge::RawHidAdmission`: a `/dev/uhid` device is created
  only for an interface whose descriptor declares a pen, of a device whose
  interfaces classify as a pen tablet. No vendor id is consulted (the 0.15
  five-vendor allowlist is gone).
- **This is not USB bridging.** It carries only raw IOHID *input* reports
  one direction (client to host); it has no control, feature, output, or USB
  transfer channel, and it cannot satisfy a vendor host driver's bidirectional
  protocol. Extending this quarantined path is explicitly out of scope; the
  true future bridge (below) is a different design, not a graduation of this
  feature flag.
- A hostile/buggy old or unknown peer that never advertises
  `experimental_raw_hid` is never admitted, regardless of local
  feature/env-var state.

## Future true USB bridge (not implemented)

Recorded here as a decision boundary, not a design in progress. A true USB
bridge — where a host owns the physical tablet's full logical USB identity
over the network — is a **separate, future, separately approved** feature
requiring all of:

- **Exclusive client device ownership**, relinquished to the host for the
  bridge's duration, with explicit user consent and an allowlist of bridged
  devices.
- **Complete bidirectional USB semantics**: control transfers, feature
  reports, interrupt/output reports, and full USB lifecycle/error signaling
  in both directions — not the one-directional input-only quarantined path
  above.
- **The tablet's driver installed on the host**, not consumed locally by the
  client while bridged — the opposite placement from local termination.
- **A reviewed Linux USB virtualization backend** (e.g. USB/IP-class
  kernel or userspace virtualization) — not `/dev/uhid`, which cannot carry
  control/feature/output transfers.
- **A production-signed Windows virtual USB/HID driver** — a synthetic
  pointer device (this document's Windows backend) cannot present full USB
  device semantics.
- **An explicit low-latency network policy/warning**: USB bridging is
  latency-sensitive in a way local termination is not, and a bridge over a
  lossy or high-latency network needs its own documented warning and
  possibly a lossy-report tolerance mode.
- **Device allowlisting, kernel attack-surface review, and a separate
  Release/Security approval** before any implementation begins — this is a
  new kernel-facing trust boundary on both host platforms, distinct from the
  quarantined raw-HID passthrough's already-bounded, input-only surface.

None of the code, wire messages, or feature flags described elsewhere in
this document implement, enable, or are a step toward this bridge.

## Behavioral notes

The tablet behavior is grounded in established remote-input semantics: real-world
pen-vs-eraser, contact, and proximity handling plus an earlier Linux uinput tablet
shape. All Rust code, types, FFI usage, bounded-queue/state-machine design, and
unit conversions are original Arcen work — no external source line, string, or
structure was copied, ported, or transliterated. The Windows backend is wholly
original Arcen work; older behavior downgraded pen input to mouse input and
never implemented native pressure.

## Operator validation

See [`../operations/pen-tablet-input.md`](../operations/pen-tablet-input.md)
for the concrete Tablet Monitor, macOS signed dev/release-mode, Linux
`libinput`/`evtest`/Krita, Windows Ink, old-peer fallback, teardown, and
logs/privacy checklist, plus the outstanding human-physical test gate.

## Cross-references

- [`shared/protocol/WIRE.md`](../../shared/protocol/WIRE.md) — full wire
  field/validation/compatibility contract.
- [`shared/input/ARCHITECTURE.md`](../../shared/input/ARCHITECTURE.md) —
  semantic `PenEvent`/`PenTool`/capability contract.
- [`clients/macos/ARCHITECTURE.md`](../../clients/macos/ARCHITECTURE.md) —
  "Tablet / pen input (typed local termination)" and "Signing and
  distribution" (entitlements/TCC, unrelated to this feature's capture path).
- [`clients/macos/SETTINGS.md`](../../clients/macos/SETTINGS.md) —
  `tablet_input_enabled` persisted setting.
- [`hosts/linux/ARCHITECTURE.md`](../../hosts/linux/ARCHITECTURE.md) —
  "Typed pen/tablet input" host section.
- [`hosts/windows/ARCHITECTURE.md`](../../hosts/windows/ARCHITECTURE.md) —
  "Typed pen/tablet input" host section.
- [`hosts/windows/todo_later.md`](../../hosts/windows/todo_later.md) — the
  future true-USB-bridge roadmap entry.
- [`legal/ORIGINS.md`](../../legal/ORIGINS.md) —
  provenance record.
