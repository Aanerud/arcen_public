# shared/input — `arcen-input`

`arcen-input` is the platform-free input contract shared by the Deck and both
active Piers. It contains no transport, async runtime, native handles, capture
APIs, or injection APIs and forbids unsafe code.

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
