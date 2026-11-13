# shared/input — `arcen-input`

`arcen-input` is the platform-free input contract shared by the Deck and the
Linux, Windows and macOS Piers. It contains no transport, async runtime, native
handles, capture APIs, or injection APIs and forbids unsafe code.

## Keyboard, text and cursor

- `keyboard` is the policy between a Mac's keys and a host's keys:
  `resolve_keyboard_policy` turns the host's platform (from its hello), what
  the host accepts and a connection's settings into a `KeyboardPolicy`
  (Windows App style, ⌘ and Control swapped, ⌘ is Control; AltGr on right or
  both ⌥; the ISO key), `ShortcutTranslator` applies it to physical key edges,
  and `keyboard::scancode` maps key identifiers to PC set-1 scan codes for a
  host that injects by scan code. The Deck's `ui/keyboard.rs` is only the
  egui adapter.
- `text` is committed text at both ends: `policy` decides per keystroke
  between the key position and the characters (Physical Keys or Characters
  mode), `keymap` and `layout` index a host layout and its active group so a
  host can prove a character typeable by key, `plan` splits one commit into
  what goes by key, what goes by the host's text route and what is refused,
  `paste` is Paste as Keystrokes (bounds, pacing, character classes) and
  `route_char`, the per-character rule every Pier uses, and `releases`
  recovers key releases an input method swallowed.
- `cursor` carries host cursor images: `HostCursor` (X11 and Windows, which
  read their one pointer) and `CursorAggregate` (Wayland capture events) feed
  a per-desktop `CursorImageRegistry` that gives each distinct image a stable
  id; `CursorLaneOutbox` sends the newest state on the input/cursor lane and
  `DeckCursorState` applies it on the Deck. Named shapes
  (`shape_for_x_cursor_name`) remain the fallback for a Deck that does not
  take images.
- `InputLaneOutbox` holds what the bounded input/cursor lane queue could not
  take, in order: edges are never dropped, merged or reordered, and only
  motion behind motion of the same kind is coalesced.

## Contracts

- `PointerMotionMode` separates absolute coordinates from relative deltas.
- `CursorMode` separates local cursor rendering from host-captured rendering.
  Cursor mode is negotiated once per connection; pointer motion mode may change
  during a connection.
- `InputCapabilities` reports absolute pointer, relative pointer, host
  cursor, and pen truth (`pen`, `pen_pressure`, `pen_tilt`, `pen_rotation`,
  `pen_eraser`, `pen_proximity`) independently. Unknown capability never
  authorizes a feature.
- `PenTool` (`Tip` \| `Eraser`) and `PenEvent` are the canonical, semantic pen
  sample: normalized `x`/`y`, `pressure` (`0..=1`), `tilt_x_degrees`/
  `tilt_y_degrees` (`-90..=90`), `rotation_degrees` (`0..=360`, both bounds
  denote the same physical angle), tool, proximity/touching, a button
  bitset, and the shared `LowLatencyMetadata`. `PenEvent::validate()` fails
  closed on any non-finite or out-of-range field.
- `pen_axes` is the only conversion into and out of that vendor-neutral
  contract: device logical ranges (`LogicalRange`, `normalize_logical`,
  `scale_unit`, `tilt_logical_to_degrees`, `twist_logical_to_degrees`),
  platform-normalized tilt (`tilt_unit_to_degrees`/`tilt_degrees_to_unit`),
  rotation wrapping, and whole-degree host axes. Host device ranges are named
  constants (`TYPED_PEN_PRESSURE_RANGE` `0..=65535`,
  `WINDOWS_INK_PRESSURE_RANGE` `0..=1024`); no tablet maker's maximum is
  assumed anywhere.
- `TabletMode::UsbTabletBridge` is Native tablet (USB bridged) for any pen
  tablet; it serializes as the legacy token `wacom_usb_bridge`, matching
  `arcen_protocol::messages::TabletModeMsg`, and converts to and from it with
  `From`.
- `virtual_device::SessionInputIdentity` names a host's virtual input devices
  for the session's dedicated X display (`Arcen Virtual Tablet Pen (session
  :58)`, vendor `a2ce`) and renders the X server input
  classes that admit only those devices: one class ignores every device, the
  next re-admits `a2ce:*` devices carrying the session token. The closing
  parenthesis keeps `:6` from matching `:61`. It performs no device or file
  operations.
- `InputSequenceTracker` accepts legacy sequence zero without advancing state
  and otherwise enforces one strict sequence across every input event type,
  including pen samples.
- `FractionalMotionAccumulator` retains subpixel native deltas without
  allocation and emits bounded signed `i32` movement.
- `RegionCoordinateTransformer` is a pure, allocation-free lookup and
  coordinate service over `arcen-media`'s validated `AppliedRegionSet`. It maps
  exclusive-bound region-local 1/120-logical-pixel input to applied pixel
  indices `0..width-1` / `0..height-1` and back across every output
  rotation/reflection and negative origin. Mapping uses the descriptor's
  explicit physical stream size; `Scale120` remains presentation metadata.
- `RegionInputState` is the one semantic aggregate for the dormant
  region-scoped input wire. It validates the current `AppliedRegionSet`
  generation/id and exclusive local logical bounds before atomically advancing
  one nonzero sequence. It owns pointer focus/active region/latest position,
  held buttons, current pen state, and deterministic `release_all()` output.
  Button and scroll edges must repeat the latest authoritative pointer position.
- `RegionInputWireMessage`/`RegionInputWireRef` are the one paired encoder and
  decoder for the input-v4 `Region*` family. `encode()` turns an accepted
  `RegionInputEvent` plus transport metadata into the exact protocol DTO, and
  `decode()` is its inverse: it validates the DTO, converts the wire generation
  and region identity into checked `arcen-media` newtypes, and yields the
  canonical event. `RegionInputWireRef<'a>` borrows an already-owned DTO so the
  decode-heavy host path never clones a message. `from_json_value`/
  `from_json_str` and `decode_json_value`/`decode_json_str` frame the family by
  its `type` tag. No product builds or interprets an individual `Region*Msg`.
- `RegionInputPipeline<M>` is the whole host-side region input path: wire
  validation, checked wire-to-domain conversion, the ordered `RegionInputState`
  transition, and the `RegionCoordinateTransformer` lookup. The only platform
  seam is `RegionPointMapper::map_applied`, which turns one applied pixel index
  into an OS-native injection point. State advances only after mapping
  succeeds, so a point a platform cannot represent never desynchronizes the
  shared stream. `validate_aggregate_parity` proves a requested `RegionSet` and
  an applied `AppliedRegionSet` describe exactly the same regions.
- `RegionInputEmitter` is the exact mirror for the sending endpoint. It owns
  the ordered state, derives the enter/leave/motion transitions a region change
  implies, allocates the strictly increasing sequence, and encodes each
  accepted transition. It borrows the applied aggregate per call so a client
  can swap the negotiated aggregate on renegotiation without rebuilding it.
  `advance_sequence_to` raises (and never lowers) the allocation floor, so a
  client whose input sequence is one session-global counter shared with its
  keyboard and legacy pointer paths keeps a single ordered stream instead of
  running a second counter beside the emitter's.

Wire DTOs remain in `arcen-protocol` (`PenToolMsg`, `PenEventMsg`/`PEN_EVENT`,
the `Region*Msg` region-input family,
`InputCapabilitiesMsg`'s pen fields — negotiated at `input_protocol_version =
3`; see `shared/protocol/WIRE.md`); products use this crate's shared ordering,
accumulation, and canonical `PenEvent` while keeping native input injection
and cursor capture inside the Linux/Windows host and capenc boundaries.
`arcen-input` depends on `arcen-protocol` solely so the `Region*` encode/decode
pair can live in one place; `arcen-protocol` must never depend on
`arcen-input`. Outside the region family a product still performs the checked
conversion between `PenEventMsg` and `PenEvent` at its own boundary.

Region-input product migration is no longer adapter-owned. A host implements
`RegionPointMapper` and calls `RegionInputPipeline`; a client calls
`RegionInputEmitter`. Both still inject or mirror every item returned by
`release_all()` on focus loss, disconnect, or topology replacement.

See [`docs/architecture/pen-tablet-input.md`](../../docs/architecture/pen-tablet-input.md)
for the full local-termination design (macOS AppKit capture, Linux `uinput`
and Windows synthetic-`PT_PEN` injection backends, the quarantined
experimental raw-HID passthrough, and the future true-USB-bridge boundary)
that consumes these contracts.
