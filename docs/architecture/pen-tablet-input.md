# Typed pen/tablet input (local termination)

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

During validation a critical capture bug was found and fixed: Wacom on macOS
delivers pen motion as **mouse/drag events with a `TabletPoint` subtype**,
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
absence of the known-bad vendor.

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
Wacom driver already installed on the macOS client decodes the physical
tablet, AppKit delivers typed pressure/tilt/rotation/tool/proximity events to
Deck, Deck maps and forwards one small typed sample over the negotiated input
protocol, and each active Pier injects a native virtual pen device that any
ordinary Linux or Windows creative application already understands. Neither
host runs, needs, or requires a Wacom driver.

**True USB bridging** — a future feature where a host owns the physical
device and full bidirectional USB semantics traverse the network — is a
different feature, driver, latency, and security boundary. It is **not**
implemented, is **not** what the raw-HID quarantine or its future removal
enables, and is described only as a scoped-out future decision boundary
below (["Future true USB bridge (not implemented)"](#future-true-usb-bridge-not-implemented)).

## Data path

```text
Wacom hardware + macOS Wacom driver
  -> AppKit NSEvent tabletPoint / tabletProximity
  -> clients/macos/src/tablet/monitor.rs   (main-thread AppKit local event monitor, RAII)
  -> clients/macos/src/tablet/mapper.rs    (pure native-sample -> arcen_input::PenEvent mapper)
  -> clients/macos/src/tablet/dispatch.rs  (edge-preserving, motion-coalescing, bounded dispatch)
  -> arcen_protocol::messages::PenEventMsg (negotiated input protocol v3; PenEventMsg::validate())
  -> Linux hosts/linux/src/input/uinput.rs "Arcen Virtual Tablet Pen" uinput device
     OR Windows hosts/windows/src/input.rs PenInjector (synthetic PT_PEN pointer device)
  -> remote creative application (Krita, GIMP, Photoshop, ...)
```

No Wacom SDK, vendor report parsing, or proprietary dependency is used
anywhere in this path. AppKit — not raw HID report decoding — is the capture
boundary; Wacom's own driver remains responsible for device-specific decoding
and pressure curves, while Arcen owns a small, stable, vendor-neutral pen
contract (`arcen_input::PenEvent`, `arcen_protocol::messages::PenEventMsg`).

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
  WAN-recommended mode. It becomes active only when Deck detects a tablet and
  the input-v3 pen path is available.
- **Native tablet mode** (`wacom_usb_bridge`) is an explicit LAN/KVM choice and
  must not be conflated with generic HID/PT_PEN injection; bridge parity
  requires complete USB semantics plus the host driver stack. If unavailable,
  the host rejects it and activates `disabled_mouse_compat`, never
  `local_termination`.
- **Mouse compatibility only** (`disabled_mouse_compat`) is the fail-safe and
  user-selected compatibility mode.

## macOS capture and Deck integration

Module: `clients/macos/src/tablet/` (`mod.rs`, `sample.rs`, `probe.rs`,
`monitor.rs`, `mapper.rs`, `dispatch.rs`, `runtime.rs`); wired into
`clients/macos/src/ui/app.rs`, `clients/macos/src/ui/macos_menu.rs`, and the
pen-capability parts of `clients/macos/src/transport/websocket.rs`.

- `monitor.rs` is a main-thread-owned AppKit local `NSEvent` monitor RAII
  guard (`objc2`/`objc2-app-kit`/`block2`; no Wacom SDK) that delivers
  `tabletPoint`/`tabletProximity` samples into a bounded producer queue.
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
- `dispatch.rs` (`TabletEventDispatcher`) folds a drained sample batch into
  an edge-preserving, motion-coalescing, bounded output: proximity, contact,
  tool, and button transitions are never dropped; only superseded hover/move
  samples may coalesce. `release_event()`/`last_position()` emit one final
  synthetic out-of-proximity release so a remote tool can never be left
  logically stuck down.
- `probe.rs` combines two independently deterministic signals, neither of
  which claims or requires a human touched the pen: **USB presence**
  (`wacom_usb_presence()`, Wacom vendor ID `0x056A`, via the existing
  cross-platform USB inventory) and **empirically observed axis capability**
  (`TabletCapabilityProbe`: pressure/tilt/rotation/eraser/barrel-buttons each
  start `Unknown` and move to `Available` only once a delivered sample
  actually demonstrates that axis in use — never synthesized, never inferred
  from silence).
- `tablet_input_enabled` (default `true`, "Enable typed pen/tablet input
  (Wacom)" under Advanced settings) gates client pen-capability
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
  local-only diagnostic panel (never logged or persisted): Wacom USB
  presence, whether the runtime installed, negotiated state, current
  tool/proximity/touching, live pressure/tilt/rotation, lifetime
  events-sent/overflow-reset counters, producer-drop and dispatcher
  edge/coalesced/overflow counters, the empirical capability probe, and the
  last error string. It reports device/capability/liveness facts only —
  never coordinates, pressure samples, raw report bytes, or other input
  content.
- **ExpressKeys and the touch ring/strip are configured in Wacom Center on
  the client**, not by Arcen, to emit ordinary keyboard or wheel input,
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
  `ABS_PRESSURE` (inclusive `0..=8191` — the documented 13-bit
  `PRESSURE_MAX_13BIT` range, matching the magnitude Wacom's own Linux
  driver reports for modern pens such as Pro Pen 2, so the virtual device
  does not overclaim vendor-specific resolution while still preserving full
  source precision), `ABS_TILT_X`/`ABS_TILT_Y` (whole-degree `-90..=90`,
  passed straight through from the wire's degree convention), tool/proximity
  via `BTN_TOOL_PEN`/`BTN_TOOL_RUBBER`, tip contact via `BTN_TOUCH`, and two
  barrel buttons via `BTN_STYLUS`/`BTN_STYLUS2`.
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
- **No Wacom driver runs on the Linux host.** `libinput`/Xorg see an
  ordinary kernel tablet-tool device; the Linux target is the dedicated
  Arcen Xorg session described in `hosts/linux/ARCHITECTURE.md`.
- Parse/validate every `PenEventMsg` through the existing globally sequenced
  input dispatcher before mapping or emitting.

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
  range (`pen_pressure_to_windows`: `round(pressure.clamp(0,1) * 1024)`) even
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

Confirmed by physical Wacom Intuos5 touch L validation (2026-08-03).

### Supported (end-to-end confirmed)

| Capability | Linux | Windows | Notes |
|---|---|---|---|
| Position X/Y | ✅ Full | ✅ Full | Normalised → screen pixels |
| Pressure | ✅ 8191 levels (13-bit) | ✅ 1024 levels (10-bit) | See ceiling note |
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
`PT_PEN` pointer data. The Wacom hardware reports 8192 levels; AppKit and the
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
- **Tangential pressure / fingerwheel** (airbrush side-wheel) — Wacom device
  feature not exposed by AppKit `NSEvent`; only accessible via raw HID.
- **Wacom-driver pressure curves and device-specific calibration** — the
  Wacom host driver must own the physical device; local termination
  intentionally lets the macOS driver run those curves before Arcen sees the
  event.
- **ExpressKey / touch-ring software mapping on the remote host** — buttons
  and touch-ring shortcuts already work as keyboard/scroll events through
  Deck's existing keyboard/wheel path (the macOS Wacom driver handles the
  remapping); only Wacom Center host-side configuration requires the driver
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
  count (`MAX_EXPERIMENTAL_RAW_HID_DEVICES = 8`) and restricts admission to
  an explicit vendor allowlist (`is_experimental_raw_hid_vendor`: Wacom
  `0x056A`, Huion `0x256c`, XP-Pen `0x28bd`, UC-Logic `0x5543`, Gaomon
  `0x0b57`).
- **This is not USB bridging.** It carries only raw IOHID *input* reports
  one direction (client to host); it has no control, feature, output, or USB
  transfer channel, and it cannot satisfy a Wacom host driver's bidirectional
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
- **The Wacom driver installed on the host**, not consumed locally by the
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
