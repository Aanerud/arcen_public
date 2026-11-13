# ADR 0018: Unified typed devices

**Status:** Proposed 2026-11-09. Design only; nothing here is implemented.

> Touches the wire and the shared model, the trust boundary (virtual devices
> are created from peer input) and any Windows dependency on a third-party
> driver.

## Context

The Deck forwards a keyboard, a pointer and one typed pen. The pen is a
special case throughout: six `pen_*` hello fields, `PenEventMsg`, its own
outbox and release rules and host devices
([pen-tablet-input.md](../architecture/pen-tablet-input.md)). A gamepad, 3D
mouse or grading panel would each repeat that work.

The direction: send a device's properties the way a game controller
announces itself, as Moonlight does, and aim to support anything.

What prior art shows (studied for mechanism only; no code is used):

- **Moonlight and Sunshine** announce a controller with an *arrival* record:
  number, a fixed type (Xbox, PlayStation, Nintendo), a capability bitmask
  (analogue triggers, rumble, trigger rumble, touchpad, accelerometer,
  gyroscope, battery, RGB LED) and its buttons. The host picks a matching
  virtual device or falls back. State, touchpad, motion and battery are
  separate records; rumble, LED colour and "send motion at N Hz" flow back.
  Same-type records queued together are merged. Windows uses a virtual
  gamepad bus (ViGEmBus); Linux uses uinput or uhid.
- **Amazon DCV** makes uinput gamepads through a helper with a rumble pipe
  back, and ships a rebranded ViGEmBus on Windows; **Reemo** uses ViGEmBus or
  its own signed USB driver.

The closed type list is the weakness: a device that is not one of three
controller families has no way to say what it is.

## Decision

### 1. A device is announced by a typed capability descriptor

The Deck announces each local device it is willing to share with
`device_arrival` and withdraws it with `device_removal`. The descriptor is a
typed, bounded description, never a HID report descriptor:

| Field | Meaning |
| --- | --- |
| `device_id` | Deck-assigned, 1–255, unique within the attachment, kept across resume |
| `class` | `pen`, `touch`, `gamepad`, `multi_axis` (3D mouse), `control_surface` (colour panel, jog/shuttle, encoders), `generic` |
| `class_version` | per-class schema version; an unknown version is refused, not guessed |
| `profile` | optional layout hint (`xbox`, `playstation`, `nintendo`, `generic`); a hint, never a requirement |
| `axes[]` | usage, `absolute` or `relative`, logical min/max, resolution, physical unit, rest value |
| `buttons[]`, `hats[]` | canonical usages in a fixed order, so a state record is a bitset |
| `surfaces[]` | touch surfaces: contacts, logical size, `direct` or `indirect` |
| `motion` | accelerometer and gyroscope ranges and the maximum sample rate |
| `feedback` | rumble motors, trigger motors, LED (player index or RGB) |
| `battery` | whether level and charging state are reported |

Moonlight's controller type survives as `profile`, its bitmask as the
optional sections. A device is what it declares, as pen classification is.

### 2. The Pier answers per device with what it can realise

`device_realisation` (Pier → Deck) carries, per device: `accepted`, the
`realisation` token (which virtual device was made), a per-capability verdict
(`realised`, `emulated`, `dropped`), and a bounded `reason` when refused
(`class_not_allowed`, `no_backend`, `limit_reached`, `not_session_scoped`,
`unsupported_class_version`). A dropped axis is reported, never faked: the
same truth rule as `unknown never authorizes` for pen. The decision is one
shared function, `resolve_device_realisation(descriptor, host_offer,
policy)`; a platform only reports what its backends can create.

| Class | Linux Pier | Windows Pier | macOS Pier |
| --- | --- | --- | --- |
| pen | uinput tablet tool; per-desktop seat pen on Wayland ([ADR 0015](0015-native-wayland-desktops-on-their-own-seat.md)) | synthetic `PT_PEN` pointer (Windows Ink; no Wintab, see [ADR 0017](0017-forward-only-pen-input.md)) | `IOHIDUserDevice`/Core HID digitizer, `com.apple.developer.hid.virtual.device` |
| touch | uinput multi-touch (MT-B) | synthetic `PT_TOUCH` pointer | `dropped`: no touch desktop |
| gamepad | uinput following the kernel gamepad layout, `EV_FF` rumble read back; motion and touchpad as sibling devices, as the kernel does for real pads | see below | Core HID virtual gamepad, once proven to reach Game Controller apps |
| multi_axis | uinput relative axes or uhid multi-axis controller | `no_backend` | Core HID multi-axis controller |
| control_surface | uhid, or an ALSA sequencer port | `no_backend` unless a user-mode MIDI route is proven | Core HID, or a Core MIDI virtual source |
| generic | uhid | `no_backend` | Core HID |

**Windows has no user-mode way to create a HID device** (the Virtual HID
Framework needs a kernel driver, which ADR 0017 rules out while unfunded), so
the table is asymmetric on purpose. Gamepads try two routes, in order, and the
answer names the one that ran:

1. `Windows.UI.Input.Preview.Injection.InputInjector` gamepad injection
   (user mode, Xbox layout, no rumble return). Its reach from a desktop
   process and to XInput, GameInput and Windows.Gaming.Input readers is a
   **proof item**, not an assumption.
2. A ViGEmBus-class virtual gamepad bus the operator installed. Arcen detects
   it at run time; it never ships, bundles or installs a kernel driver.
   ViGEmBus is retired upstream, which is recorded as a risk. Neither
   present: `no_backend`, and the Deck says so.

**Device identity on the host.** Virtual devices use Arcen's vendor id
(`0xA2CE`) and a session-scoped name (`virtual_device::SessionInputIdentity`).
A well-known identity (an Xbox-compatible gamepad) is used only when a
backend requires it to be recognised at all, chosen by `profile`, and
reported in the realisation. The Deck's own vendor, product and serial never
reach the host.

### 3. Events are typed, versioned, coalescable records on the input lane

All device traffic travels on the input/cursor lane
([ADR 0014](0014-quic-lanes-and-capture-contracts.md)) and shares the one
global input sequence with keys, pointer and pen, so order between a key and
a button press is kept.

- **Arrival, realisation, removal** are JSON `SessionRecord`s.
- **State records** are a binary input-lane frame (like the `0x50` cursor
  update): version, `device_id`, flags, `sequence`, `timestamp_ns`, then the
  class payload — button bitset, hats, axis values in the *declared logical
  range* (native resolution, converted on the host by shared code in the
  manner of `pen_axes`), contacts. A value outside its declared range rejects
  the record before it touches state.
- **Motion** and **battery** are their own records. Motion is off until the
  Pier asks for it at a rate (`device_feedback{motion_rate_hz}`), which it
  does only when a consumer is present (uhid open) or the user enabled it.
- **Feedback** (Pier → Deck): rumble intensities with a duration capped at
  5 s, trigger rumble, LED, player index, motion rate. The Deck stops every
  motor when the lane, the attachment or the window focus is lost.

Coalescing generalises the pen rule in `InputLaneOutbox`. A record whose
buttons, hats, contacts, tool or proximity changed is an **edge**: never
dropped, merged or reordered, so a press and release inside one frame
interval are two edges. A record that only moved continuous values is folded
only under real lane back-pressure: absolute axes and motion newest-wins,
relative axes summed (as `mouse_move_relative`).

**Release.** `input_lane_recovery` and detach put every realised device in
its neutral state (buttons up, sticks at rest, triggers zero, contacts
lifted, pen out of proximity). A resume lease keeps the virtual devices for
the lease window, so a game sees no unplug on a reconnect; expiry removes them.

**Versioning.** `input_protocol_version` 5 enables typed devices. Both peers
must advertise it plus `typed_devices = available`; a v4 peer sees none of
these records. Unknown descriptor sections are returned as `dropped`, so a
newer Deck can announce a richer device to an older Pier.

### 4. Pen is the first device class; keyboard stays outside

- **Pen.** Between v5 peers the Deck announces its pen as `class: pen`. Its
  events stay `pen_event`, unchanged but for an optional `device_id` (absent
  = the one implicit pen), so a second pen needs no new message. The hello's
  six `pen_*` fields are
  still sent, derived from the pen's realisation, so v3/v4 peers behave
  exactly as today. `PenForwardingDispatcher`, `plan_pen_edges` and
  `pen_axes` become the pen class's implementation, not a parallel path.
- **TabletMode is unchanged.** Tablet support (`local_termination`) is "the
  pen device is realised". Mouse compatibility is "no pen device".
- **Keyboard and the session pointer are not typed devices.** Keys keep
  `key_event`, the keyboard policy and `text_commit`. A descriptor that
  declares keyboard usages is refused: a second keyboard path would bypass
  shortcut translation and the host's keymap proof. Tablet express keys keep
  arriving as keys from the client's tablet driver.

### 5. Not Native tablet

| | Typed devices (this ADR) | Native tablet (USB bridge) |
| --- | --- | --- |
| Device decoded by | the Deck's OS and its driver | the host's driver |
| On the wire | typed descriptor and state records | USB requests and HID reports |
| Host sees | an Arcen virtual device | the physical device's own identity |
| Windows host | yes, within user-mode APIs | not offered (ADR 0017) |
| Network | WAN-safe, coalescable | LAN only, latency-sensitive |
| For | everything with a typed class | vendor protocols: driver pressure curves, vendor control panels |

Panels speaking a maker's USB protocol stay bridge work; no raw escape hatch.

## Security and privacy

- **No arbitrary HID passthrough.** The Deck never sends a report
  descriptor. Every host descriptor is built by shared code
  (`hid_reports`) from the typed descriptor and checked with the shared HID
  parser (`arcen-usb-bridge`) before a kernel sees it.
- **Bounds.** Arrival ≤ 4 KiB; ≤ 8 devices per attachment, ≤ 4 gamepads;
  ≤ 32 axes, 128 buttons, 4 hats, 10 contacts, 4 motors per device; state
  record ≤ 256 bytes. Per device, a token bucket of 2,000 records/s (burst
  256): excess motion is folded; a device that floods edges is suspended
  with a reason, never silently thinned.
- **Per-device allow-list.** Pier policy `input.devices.classes` defaults to
  `pen` and `gamepad`; `multi_axis`, `control_surface` and `generic` are
  opt-in. The Deck shares a device only after the user enabled it (pen and
  gamepad on by default, per-device switch in the Input tab).
- **Session scoping.** A Linux virtual device is visible only to its
  desktop's session (session identity on Xorg; its own seat on Wayland, as
  the seat pen). Windows gamepads are machine-wide, so the Windows Pier
  realises one only for the attachment that owns the interactive console
  session, else `not_session_scoped`.
- **Privacy.** No serial numbers, Bluetooth addresses or product strings on
  the wire. Logs and telemetry carry class, counts, verdicts and latency,
  never values. Motion streams only on demand.

## Latency

Deck capture timestamp to host injection returning, minus the network
one-way time: **p50 ≤ 1 ms, p99 ≤ 4 ms** added; no edge lost or reordered;
motion at the device rate up to 1 kHz. As for the pen, a sample leaves from
the callback that delivered it, never from the UI pass.

"Better than Moonlight/Sunshine" is a measurement, not a claim: the same
client Mac, host, controller and network, with a scripted USB HID controller
emulator pressing buttons at known times and a host-side probe timestamping
the virtual device's events (evdev, GameInput) on a shared clock. Report
p50/p99 latency and lost or merged edges over 10,000 presses and a 1 kHz
motion stream, for both products. Arcen ships the gamepad class only if it
is no worse on p99 and loses no edges.

## Testability

- `arcen_input::devices` holds the model, validation, realisation,
  coalescing and neutral states, OS-free. **Shared simulators** script pen,
  gamepad, 3D mouse and panel devices into a reference host model that checks
  the invariants (no stuck button after any lane failure, edges kept under
  every back-pressure pattern, values in range); golden vectors pin the wire.
- Host adapters stay pure mappers (as `hosts/linux/src/input/pen.rs`),
  tested off-target; a per-OS read-back test opens the created device.

## Plan

| Phase | Scope | Gate |
| --- | --- | --- |
| P1 | Shared model, wire (input v5), simulators; pen moved onto it with v3/v4 compatibility; gamepad on Linux (uinput) and Windows (route 1, else 2); macOS Deck capture through the Game Controller framework (state, haptics, light, battery, motion) | read-back tests on each OS; the latency comparison above |
| P2 | `multi_axis` and `control_surface` on Linux and macOS Pier; touch on Linux and Windows; macOS Pier gamepad | each realisation proven by an application reading it |
| P3 | `generic` under policy; more than one pen | Release/Security review of the opt-in |

## Alternatives considered

- **Moonlight's fixed controller types**: every new family is a protocol
  change. Kept as `profile`.
- **Raw HID passthrough**: a peer's descriptor in front of the host kernel's
  parser, and a kernel driver on Windows (see the quarantined raw-HID path).
- **USB bridge for everything**: right for vendor protocols, wrong as a
  default — LAN-only, a driver per host, unavailable on Windows.
- **One message per class**: what the pen became; policy, limits, release,
  resume and telemetry would be rewritten for each class.

## Consequences

- A new class costs a descriptor section, a host mapper and simulator
  coverage, not a new negotiation. Windows realises fewer classes, per device
  and with a reason. The pen's wire format does not change.
- New shared surface: `arcen-protocol` messages and WIRE.md, `arcen-input`
  devices, `arcen-session` device leases, `arcen-telemetry` `input_device_*`
  events, and `consumers.json` records for every product path.

## Open questions

- Should 1 kHz motion leave the ordered input lane if it delays keys under
  loss? Which identity do SDL and Linux games accept without a mapping?
