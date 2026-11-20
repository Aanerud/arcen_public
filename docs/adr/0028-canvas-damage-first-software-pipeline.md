# ADR 0028: Canvas — a damage-first software pipeline

**Status:** Proposed 2026-11-18. The wire vocabulary (`arcen_protocol::canvas`)
and the shared interpreter (`arcen_media::canvas`: the state owner, the CPU
executor, the mirror and its digest, depth-8 tile packing) exist and are
tested, including a corpus replay in which the Pier's mirror digest equals
an independent replay of the wire bytes for every frame. The Deck's shared
half (`arcen_deck_core::canvas`, §9) runs it in the media worker. Both
Decks have an eight-bit canvas executor (the Windows Deck's D3D11 executor
and the macOS Deck's Metal executor, §10) and advertise `canvas_v1` for
single-window sessions, and every Pier's software capture runs the one
shared loop (§11) and serves canvas records to a Deck that offers them.
Amends
[ADR 0016](0016-lossless-refinement.md) §2 (Auto) and R7 for eight-bit tiles
inside canvas frames. Needs Shared/Architecture review (wire, plan truth) and
Release/Security review (a new record type decoded from untrusted input on
both ends).

> Records how a Pier that encodes in software sends only what changed, as
> ordered commands against a canvas the Deck keeps per monitor, so that its
> cost follows changed pixels rather than desktop pixels, and how both ends
> stay exact.

## Context

The Software contract (OpenH264, Constrained Baseline 4:2:0) encodes every
captured picture whole. Its cost follows the desktop's size, not what
changed: measured on an Apple M4 Pro, one word changing on a still 3840×2160
page costs about 29 ms of encoding plus 10 ms of colour conversion, more than
full-screen motion, and extra encoder threads barely help. OpenH264 runs its
own full-picture scene-change, static-block and scroll analysis on every
picture, which is the work Keel ([`arcen-keel.md`](../architecture/arcen-keel.md))
already does once. Raising the software size and rate limits alone would
move that wall, not remove it.

Surface protocols scale differently. MS-RDPEGFX, for example, keeps
surfaces on the client and sends commands against them: solid fills,
surface-to-surface copies for scrolls and moves, cache stores and paints,
codec payloads into a destination rectangle (H.264 only where content
moves, with a region mask), and frame acknowledgements that report the
client's unprocessed bytes. Arcen already has the pieces: Keel's 16×16
damage and activity classes, refine's lossless tile codec, grid and digests
(ADR 0016), the software encoder session, region-frame admission and the
relay window.

## Decision

### 1. The model

- **Deck.** Per monitor, one persistent canvas at the monitor's source
  size, opaque eight-bit BGRA (the desktop's own sRGB-coded values). The
  Deck applies canvas commands in wire order and presents the canvas, never
  a decoded picture directly. The canvas is bounded by the Deck's texture
  limit, which it advertises.
- **Pier.** A mirror of each canvas in host memory, the same type as the
  Deck's model. Every command is applied to it the way the Deck applies
  it, so the Pier knows what the user sees: copy and cache decisions,
  exactness digests and recovery are made against the mirror.
- **One interpreter.** `arcen_media::canvas` (OS-free, no native
  dependency) validates commands against the canvas and produces a list of
  GPU operations (`CanvasWork`, like refine's `OverlayWork`); its CPU
  executor is the Pier's mirror and the oracle every Deck executor is
  tested against. Platform code is a GPU executor only.

### 2. Scope

Canvas is the Software contract's pipeline only. Hardware contracts
(NvFBC → CUDA → NVENC, NVENC Grading and HDR, Windows DDA/WGC → NVENC,
VideoToolbox) keep their full-frame streams and fast paths unchanged. A Deck
that does not send `canvas_v1` keeps receiving today's full-frame records.
Speed, Grading and HDR keep their existing software behaviour (Speed served
as Auto, Grading and HDR refused).

### 3. Wire (normative; layout in `shared/protocol/WIRE.md`)

- **Capability.** `AuthResponse.canvas_v1 {max_canvas_px,
  max_surfaces_per_monitor, cache_bytes, cache_slots, tile_codecs}`; a Pier
  clamps it to the protocol bounds (`CanvasCapability::terms`) and answers
  `ServerHelloMsg.canvas {state, reason?, tile_codec?, cache_bytes?,
  cache_slots?, max_surfaces_per_monitor?, monitors[]}` with a state per
  monitor, modelled on `LosslessRefineStatusMsg`. Terms in the hello never
  exceed the Deck's.
- **One frame type, two kinds.** `Canvas = 0x0E` on the video lane. Kind 1
  is `CanvasReset`, the ordered barrier that starts a canvas generation
  (clear to opaque black, close surfaces, empty the cache). Kind 2 is
  `CanvasFrame`: scope, a nonzero fence that increases strictly within a
  generation, a wire-clock timestamp, the grid's block count and an ordered
  list of commands, applied whole before the canvas is presented.
- **Commands** are bounded type-length records: `Fill`, `Copy`, `Tile`,
  `CacheStore`, `CachePaint`, `CacheEvict`, `SurfaceOpen`, `SurfaceClose`,
  `SurfacePaint {surface, Annex-B access unit, destination rectangle, block
  runs}`. An unknown command, a nonzero reserved field or a wrong length
  refuses the whole record: the mirror must stay exact, so nothing is
  skipped.
- **Deck → Pier.** `canvas_ack {monitor_id, generation, fence,
  unprocessed_bytes, decode_ms_p95}` and `canvas_reset_request
  {monitor_id, generation, reason}` on the control lane.

### 4. Limits, enforced before allocation

| Limit | Value |
| --- | --- |
| Record | ≤ 16 MiB, checked before parsing |
| Commands per frame | ≤ 65,536 and ≤ what the record can hold at its smallest command (10 bytes), checked before the list is allocated |
| Canvas | 1..=16,384 pixels a side, so a grid of ≤ 2^20 blocks (refine's `MAX_GRID_BLOCKS`) |
| Work per frame | the area the commands write (fills, copies, tiles, cache stores and paints, painted blocks) ≤ 4 × the canvas area, so a record's cost follows the canvas, not its command count |
| Tile | refine's: grid origin, ≤ 256 × 64, expanded ≤ 98,304 B, payload 1..=131,072 B; `delta-zstd1` at depth 8 |
| Cache entry | tile geometry; slot < 65,536; the Deck's byte budget is never exceeded (the Pier evicts first) |
| Surfaces | < 4 per monitor (the Deck may advertise fewer); even sizes ≤ 8,192 a side and ≤ 139,264 macroblocks |
| Access unit | 1..=8 MiB |
| Block runs | ascending, non-overlapping, non-empty, `< block_count`, counted against the bytes present before allocation; inside the destination rectangle |
| Status | ≤ 16 distinct monitors; reason tokens ≤ 32 bytes of lowercase ASCII |

Rules that need the canvas size (a rectangle inside the canvas, grid
alignment at the right and bottom edges, runs inside the destination, a
frame's block count against the generation's, a reset's size against the
Deck's `max_canvas_px`, cache bytes) are checked by the state owner
(`arcen_media::canvas::CanvasState`), which holds the size from the
generation's reset. A refused record breaks the monitor's state until the
next reset. No product may decode canvas records without it, and every receiver calls
`CanvasStatusMsg::validate` and `CanvasControlMsg::validate` before acting
on either message.

### 5. Ordering and recovery

Canvas records ride the video lane in order with each other and are
reliable media (`ReliabilityClass::MediaReliable`): never a datagram, never
dropped on their own. A sender that must discard canvas records (a full
relay backlog, an encoder restart) discards the rest of the generation and
starts a new one with a reset (reason `recovery`). A Deck that refuses a
record, loses its device or fails a decode drops the monitor's canvas state
and sends `canvas_reset_request`; the Pier answers with a reset (reason
`reset request`) and repaints. This is ADR 0013's recovery for a canvas: a
reset and a full lossless or surface repaint replace a desktop keyframe.
Canvas fences are on ADR 0026's single timeline.

### 6. Exactness

`Fill`, `Copy`, `Tile` and the cache commands are applied bit-exactly to the
mirror and the Deck's canvas. `SurfacePaint` is lossy; the mirror marks the
painted blocks lossy. By default lossy blocks are never `Copy` or
`CacheStore` sources, so every copy moves exact pixels; decoding surfaces
on the Pier to make lossy areas copyable is a later option to be decided
from measurements. Exactness is claimed only for lossless blocks and
verified with refine's canonical digest (ADR 0016 R4) over the canvas's
codes: contract eight-bit R′G′B′ (sRGB transfer, BT.709 primaries, full
range), the canvas scope and fence, every block not marked lossy. The state
owner tracks lossiness per block: a surface paint marks its blocks, a copy
or cache paint carries the flags of its source blocks, a fill or tile over
a whole block clears it, and a partial write only adds to it.

### 7. Flow control

The Pier-side relay window still bounds frames in flight. `canvas_ack`
adds what the Deck has presented and the bytes it holds unapplied, once
per presented frame and at least every 250 ms while frames arrive. It is
used to throttle optional work only (at most two unacknowledged frames per
monitor carry optional work), never for cache or canvas correctness. A
throttled frame loses nothing: Keel's pending damage merges into the next.
An acknowledgement of another generation, or of a fence the generation has
not sent (an earlier loop's generation of the same number, after a capture
restarted for a new size), is stale and changes nothing.

The Pier's own send queues apply backpressure, not loss
(`arcen_media::video::OutboundVideoRecord::room_wait`): a canvas frame's
producer waits for room however long the path stalls, because a dropped
record costs a new generation (a full repaint, larger than whatever a short
stall held back) while waiting loses nothing; capenc then stalls on its
pipe and the loop merges the damage it could not send. A queue holds at
most its record bound and `CANVAS_QUEUE_BYTES` (one record of the largest
size). A reset never waits: it supersedes what its generation queued. Only
a closed queue (teardown, which releases a waiting producer at once), the
Windows Pier's legacy queue (`ARCEN_VIDEO_BACKLOG=legacy`, whose reader
and router do not wait at all, as before this rule) or the Windows relay's
own backlog drops a record, and that genuine loss starts a new generation.
The Linux Pier's legacy queue only drops the backlog budget: a canvas frame
there still waits for the record and byte bounds.

### 8. Amendment to ADR 0016 (§2 Auto, R7)

Eight-bit lossless RGB tiles are allowed **inside canvas frames** for the
Software contract. They reuse refine's `delta-zstd1` at depth 8, its tile
limits and its canonical digest, through an eight-bit BGRA `RefineSource`.
They travel on the video lane inside the canvas frame, ordered with surface
paints, and are granted by `canvas_v1`, not by `lossless_refine_v1`.
Refine's ten-bit plan, its refine lane and its R7 gating are unchanged;
canvas does not grant the refine lane.

### 9. The Deck's shared half

`arcen_deck_core::canvas` is the whole Deck side except the GPU calls:

- **Advertisement.** `CanvasRequest::for_sink::<P>` builds `canvas_v1` from
  the presenter's `VideoPresenterSink::CANVAS_EXECUTOR` (its texture limit)
  and the user's switch (Settings, on by default, and per connection). A
  sink that states no executor makes the request `Unavailable`: nothing is
  advertised and the switch is not shown. Surfaces: at most two per
  monitor. Cache: 100 MiB by default (`canvas_cache_mib`), never below
  16 MiB, one slot per 4 KiB.
- **Admission.** A record is applied only for a monitor the hello lists
  `on` (the hello is checked with `CanvasStatusMsg::validate` against what
  the Deck offered). A single-monitor session takes monitor 0 with
  topology generation and stream epoch 0, as its legacy video records; a
  multi-monitor session uses region-frame admission (the committed topology
  generation, a monitor of the roster, its stream epoch). A record that is
  not admitted is stale and dropped; it never breaks a canvas. The one
  exception is a multi-monitor record that arrives before the Deck has
  committed its roster: its canvas is marked lost, so the Pier repaints it
  once the roster admits the answer.
- **Execution.** The state owner validates the record and keeps fences and
  generations; the Deck decodes tiles and surface access units (one decoder
  per surface from the Deck's `VideoDecoderFactory`, opened, replaced and
  closed with the surface) and hands the work to the monitor's eight-bit
  presenter (`VideoPresenterSink::execute`), which applies it, orders every
  write before the composite that samples the canvas (the ADR 0016 R2 rule)
  and presents. A hidden surface still applies the work. The Deck holds at
  most 64 MiB of decoded tiles at a time: a record with more reaches the
  presenter in parts of the same fence, and only the last part presents, so
  the record is still shown whole. A surface's decoder is created at its
  first paint after an open, so opens that are never painted cost nothing.
  Neither changes what the wire allows.
- **Inbox.** Canvas records queue in lane order beside video. A reset
  supersedes what its monitor has queued; past four maximum-size records
  the monitor's queue is discarded and its canvas waits for a reset.
- **Recovery.** A refused record, a failed decode or executor, a canvas
  without a presenter or a discarded queue loses the monitor's canvas: later
  records wait for a reset, and `canvas_reset_request` (the state owner's
  refusal token as the reason) is sent once a presenter exists, at most
  every 2 s. A hidden or not yet attached sink is not a failure: it
  applies the work and shows the canvas when it can. A sink that cannot
  show a canvas for the rest of the connection stops canvas without asking
  for resets (each would be a full repaint): a presenter without an
  executor, a sink that failed to apply two resets in a row
  (`SINK_FAILED_RESETS`), or an eight-bit presenter that fell back (a
  canvas has no keyframe to ask for). Stream Statistics then say why and
  that turning Canvas screen updates off and reconnecting gives full
  frames; switching such a connection to full frames by itself is not
  implemented yet.
- **Flow control.** `canvas_ack` with the newest applied fence after each
  batch that presented a frame, and at least every 250 ms while records
  arrive; `unprocessed_bytes` is what the inbox holds for the monitor and
  `decode_ms_p95` the surface decode time over the last second. A session
  that asked for the canvas wakes the media worker every 250 ms
  (`SessionEvent::CanvasTick`), so acknowledgements and reset requests never
  wait for the next record.
- **Digest.** At most every 10 s per monitor the presenter reads its canvas
  back; the Deck computes the canvas digest (§6) against the state at that
  fence, logs it (`canvas digest`) and keeps it in the canvas statistics, so a lab
  compares it with the Pier mirror's digest. The protocol has no record that
  carries a Pier digest to the Deck yet.
- **Statistics.** "Screen updates: canvas (tiles x%, surface y%, copy z%)"
  over the last 5 to 10 s, or why full frames arrive. Canvas records feed
  the received side of Stream Statistics as video packets do (the wire
  line, the receive rate and a frame's wire-clock delay); none counts as a
  decoded or presented frame.
- **No keyframe requests.** A connection whose hello turns the canvas on
  never sends `request_full_frame` for its first picture or after it: its
  first picture is the reset, and its recovery is `canvas_reset_request`.
  The root picture waits for a "keyframe" only while its canvas is lost.
- **Executors.** Windows Deck (`clients/windows/src/present/canvas.rs`): one
  `B8G8R8A8_UNORM` texture per monitor on the eight-bit presenter's D3D11
  device; fills and surface block spans are instanced rectangles,
  copies `CopySubresourceRegion` (overlapping ones through a scratch
  texture), tiles `UpdateSubresource`, cache entries one texture per slot;
  a surface paint converts the decoder's NV12 picture with the presenter's
  own conversion pass (through the decoded-picture hand-over when the
  decoder has its own device) and samples it by nearest neighbour into the
  listed blocks. The canvas is presented by copying it into the eight-bit
  swapchain, never between the parts of a record. Its tests compare the
  read-back canvas with the CPU executor over hand-made records and the
  canvas planner on Keel's scenarios: lossless blocks bit-exact, surface
  blocks within one code (WARP and hardware adapters). Only the root
  window executes the canvas, so a multi-window session asks for full
  frames until secondary windows register theirs.

### 10. The macOS Deck's executor

`clients/macos/src/ui/canvas_metal.rs` performs `CanvasWork` and nothing
else: one persistent private `BGRA8Unorm` texture per monitor, owned by the
connection rather than the window's layer.

- **Writes.** `Reset` is a render pass that clears to opaque black; `Fill` a
  scissored draw of the colour; `Copy` a blit, through a scratch texture
  when the rectangles overlap; `Tile` a blit from one shared staging buffer
  per record (never a CPU write into the canvas, which a GPU command in
  flight may still read); cache entries are private buffers sub-allocated
  from tracked heaps. `SurfacePaint` imports the decoded `CVPixelBuffer`'s
  planes without a copy and converts them with the shared `VideoUniform`
  (the eight-bit layer's own conversion), one scissored draw per listed
  block, each pixel taking the picture's nearest sample as the CPU executor
  scales.
- **Order.** A record's writes are one command buffer on the canvas's
  queue. The Auto/Speed layer presents the canvas texel for texel on the
  same queue under the canvas's ordering lock, and only when the canvas
  holds whole records (a part that does not present leaves it incomplete).
  A canvas frame is shown again when the layer is attached or the window
  shown, because a still desktop sends no record.
- **Readback.** On request the canvas is copied to a shared buffer at the
  end of the record's command buffer, which the Deck waits for (at most
  every 10 s).
- **Proof.** The canvas read back after every record equals
  `CanvasPixels` on `arcen_media::canvas::corpus` (the planner over Keel's
  scenarios and a cached dialog): bit-exact on every lossless block, within
  one code on surface blocks, where both sides round the same conversion.
- **Scope.** The canvas is shown by the root window's eight-bit layer, so
  a multi-monitor connection asks for none and is served full frames. A
  secondary window needs its own eight-bit layer, registered in
  `SharedMediaState::canvas_presenters`, before it can.

### 11. The Pier's half

`arcen_media::canvas::CanvasEncodeLoop` is the one software loop of every
Pier (Linux Xorg `XShm` with `XDamage` rectangles, Linux Wayland `PipeWire`
with its damage regions, Windows WGC). A capture adapter hands it pictures
and OS damage and writes its records; it replaced the separate Keel-gated
loops each path had.

- **Decision.** `canvas::serve::plan_canvas`, once per capture: on only for
  a Deck that offered `canvas_v1`, the Software contract (OpenH264,
  eight-bit 4:2:0), one monitor (monitor 0, topology generation and stream
  epoch 0) within the Deck's canvas size; otherwise off with a stable
  reason token (`not_software`, `multi_monitor`, `canvas_too_large`,
  `unsupported_terms`, `not_built`). The hello's `canvas` is that plan.
  Hardware contracts never reach the loop.
- **Older Decks.** The same loop's degenerate path: the whole monitor is one
  surface of the same encoder, sent as ordinary access units and gated by
  Keel's damage and idle cadence. There is no second software path.
- **Encoder.** One shared `SoftwareH264Session` per surface (BT.709 limited
  range, what every Deck decodes into its canvas), opened through
  `video::open_software_loop`; surfaces larger than OpenH264's own limit are
  not served (outputs stay fitted to it). A surface is encoded in OpenH264's
  camera mode (Keel already sends text as tiles; 1.2–1.5× cheaper than
  screen-content mode on Keel's drag and video scenarios), at the
  governor's grant (not the Software contract's floor, which belongs to a
  stream that owns the link) and at the governor's surface rate, so the
  grant is spread over the pictures the surface actually gets. Pictures
  share the link by their part of the monitor's area at most (the bits per
  pixel a full-frame stream would spend), so a window's surface does not
  spend the whole link on its region. Below
  OpenH264's highest QP a surface cannot go lower; the governor's rate and
  scale steps take it further.
- **Surface hysteresis.** Opening a surface costs an encoder and an IDR, so
  a surface is kept while its motion fits and covers more than half of it,
  moves (no IDR) when the motion fits its size elsewhere, and opens with an
  eighth of the motion's size (at least 32 pixels) of room on each side, so
  a growing region (a dragged window) reopens a few times instead of every
  tick. A reopen with nothing to paint keeps the surface it would replace.
- **Plumbing.** One `capenc` argument (`canvas=v1:…`), framed-v2 `Canvas`
  records back, `CANVAS-ACK`/`CANVAS-RESET`/`CANVAS-INPUT` control lines in.
  `CANVAS-INPUT <monitor> <topology_generation> <x> <y>` comes from the
  pointer and pen input the Pier authorised and applied (absolute moves,
  buttons with their position, pen samples), with the event's position in
  1/65536ths of the picture: the shared `canvas::serve::CanvasInputFeed`
  keeps one position and forwards it only when it moved 1/256 of a side or
  is 125 ms old, and capenc keeps only the newest
  (`capenc_control::CanvasControlInbox`), so input never queues and never
  displaces a reset request. It carries no input content. The loop ignores
  a line for another monitor or topology generation.
  A Pier stamps each frame with its wire clock. Its send queue and the
  Windows lane relay treat a reset as the recovery point: a dropped record
  holds its monitor until the next reset, which the keyframe request starts
  (reason `recovery`), so no generation is ever relayed with a gap. A slow
  path never drops one (§7): the Linux frame pump and the Windows capenc
  reader and frame router wait for room.
- **Evidence.** The loop logs its mirror digest at every fence that is a
  multiple of 30 (`canvas mirror digest …`); `examples/canvas_verify.rs`
  is a Deck for one capture that applies the records with the CPU executor,
  acknowledges them and logs the digest at the same fences.

## Consequences

- One shared interpreter serves the Pier's mirror and both Decks; a second
  Deck needs a GPU executor of `CanvasWork`, not new logic.
- Software cost follows changed pixels: idle monitors cost a keepalive,
  typing costs a few tiles, scrolling a copy and a strip.
- A new record type is a trust boundary on both ends; its decoders bound
  every count and length before allocating and are covered by truncation,
  extension and mutation tests.
- Frame type `0x02`, a partial-frame type no product sent, is removed and
  reserved.

## Alternatives rejected

- **Full-picture H.264 with per-macroblock skip hints.** Depends on the
  encoder: OpenH264 has no per-macroblock input, and even with one the
  whole picture is still converted and deblocked.
- **A frame type per command.** Spends nine frame-type values and splits
  one atomic update across records; one record with typed commands keeps a
  frame atomic.
- **Commands as JSON on the control lane.** Unordered with video and too
  large for tiles and access units.
- **Skipping unknown commands.** The mirror would no longer equal the
  canvas; a newer command set takes a new record version.
