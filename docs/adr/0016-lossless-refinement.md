# ADR 0016: Lossless Refinement of Still Regions

**Status:** Proposed 2026-11-08; amended after design review 2026-11-09
(see [Review 2026-11-09](#review-2026-11-09) — its rules are normative and
replace any conflicting sentence in sections 1–12; the
[first-slice amendments](#first-slice-amendments-2026-11-13) refine them).
**First slice complete 2026-11-13:** Linux Xorg depth-30 Grading/HDR to the
macOS Deck, one monitor, end to end to verification level 3, compiled into
release builds and switched at run time only (Pier
`video.lossless_refinement`; Deck setting **Exact still image**, default on;
negotiated per session). **On by default from 0.19.1:** the Pier key
defaults to `enabled`. It engages only where the shared plan allows it
(Grading or HDR, HEVC 4:4:4 ten-bit, a CPU refine source and one monitor:
Linux Xorg depth-30 desktops, Linux Wayland Grading and HDR on a
compositor with wide screen sharing, Windows Grading and HDR, and macOS
Grading and HDR); every other session reports its off-reason in the hello.
**macOS Grading and HDR source, Y′CbCr domain, 2026-11-13** (see
[the macOS amendments](#macos-amendments-2026-11-13)): the `xf44` surface
VideoToolbox encodes (after the Core Image white stage for HDR) is refined
as Y′CbCr 4:4:4 codes, composited through the presenter's own range and
matrix, verified to level 2.
**Windows Grading and HDR source 2026-11-13:** the FP16 scRGB capture,
converted to the stream's own codes by the shared `ScrgbSdrTransform` /
`ScrgbPqTransform`, mirrored exactly and checked against the capture API's
damage; the split Pier's lane relay carries fences and tiles. **Wayland Grading and HDR source 2026-11-13** (see
[the Wayland amendments](#wayland-amendments-2026-11-13)): the wide
composition's host read-back is the source, in the stream's own codes
(BT.709/sRGB SDR for Grading, PQ/BT.2020 for HDR), verified digest-exact on
the Deck's PQ/EDR presenter too. Needs Shared/Architecture
review (wire, lane role, plan truth) and Release/Security review (new lane,
new native dependency, untrusted input decoded on the Deck).

> Records how a Pier makes a still picture on the Deck *bit-exact* to the
> captured source — not "visually lossless" — in the stream's own colour
> space and bit depth, driven by Keel's 16×16 damage, without touching the
> eight-bit Speed fast path, and how both ends prove that it is exact.

## Context

A Grading or HDR stream is HEVC 4:4:4 10-bit. It is very good, but it is
lossy: a colourist who stops on a frame to judge it is judging the
encoder's approximation of the picture, and nothing on the Deck says how
far that approximation is from the source. The more a still picture
matters, the less a lossy codec is the right tool for it.

Established remote-workstation products answer this with a *refinement
layer*: once a region stops changing, it is sent again losslessly, within a
bandwidth cap, and the client paints it over the video. Learning from them
(not from their code):

- One product refines over a 32-pixel change map, eight-bit only, with JPEG
  or LZ4 tiles and a fixed Mbit cap on refinement traffic. It has no 10-bit,
  PQ or BT.2020 path.
- One product sends lossless tiles that may be 10-bit but never HDR, offers
  a tile-colouring diagnostic overlay, and checks colour by comparing the
  lossy video with lossless tiles on flat areas, accepting "within one code
  on 95% of pixels".
- One protocol progressively builds to lossless against a configurable
  target time.

None of them proves exactness: "lossless" is a claim about the tile codec,
not a measured property of what the viewer sees.

Arcen already has the pieces a better design needs:

- **Exact damage at 16×16.** Keel hashes every 16×16 block
  (`arcen_keel::BLOCK_SIZE`, `shared/keel/src/grid.rs:4`;
  `DamageTracker::update`, `shared/keel/src/damage.rs:59`), and the Linux
  ten-bit NVENC path already runs it on the CPU copy of each captured frame
  (`QpDamage::CpuHash`, `hosts/capenc/src/nvenc_cuda.rs:190`). OS damage
  feeds the same grid elsewhere (`arcen_keel::ExternalDamage`, used by
  `hosts/capenc/src/win.rs:1509` and `hosts/macos/src/stream.rs:3442`).
- **Explicit pipelines.** Grading and HDR are complete contracts with
  their own capture providers (root `AGENTS.md`, "Streaming pipeline
  boundary"); the integer code values each one encodes are produced by
  shared, source-specific conversions in `arcen_media::video::convert`.
- **Isolated lanes.** Video, audio, input and bulk traffic already run on
  separate QUIC streams with their own priorities (ADR 0014,
  `arcen_protocol::lanes::LaneRole`, `shared/protocol/src/lanes.rs:222`).
- **A dedicated 10-bit presenter.** The Deck converts decoded planes to
  RGB10 in one Metal pass it owns (`fs_convert`,
  `clients/macos/src/ui/video_metal_layer.metal`), separate from the eight-bit
  egui path, and retains the last presented frame
  (`clients/macos/src/ui/video_metal_layer.rs:2514`).

## Decision

### 1. What "exact" means: the stream's own code values

Refinement reproduces, bit for bit, the **integer code values the encoder
was given**, before any chroma subsampling and before the encoder's own
loss — in the stream's transfer, primaries and bit depth. It never introduces
a matrix, transfer or primaries conversion of its own. Each contract has
one *refinement source*, the last integer stage before the codec:

| Pier / contract | Captured buffer | Refinement source (domain) | Why this stage |
| --- | --- | --- | --- |
| Linux Xorg Grading (depth 30) | XShm packed RGB10 (`WideX11Frame`, `hosts/capenc/src/linux_x11.rs:251`, read by `capture_wide_diffed`, `:685`) | R′G′B′ 10-bit codes as captured; for a declared `rec2100-pq` desktop served as SDR, the codes after `Rgb10Signal::PqBt2020ToSdrBt709` (`shared/media/src/video/convert.rs:1366`) | These are the desktop's own pixels; the encoder's Y′CbCr is a rounded function of them. Sending RGB avoids the matrix's rounding entirely. |
| Linux Xorg HDR (operator-declared `rec2100-pq` desktop) | XShm packed RGB10 holding PQ codes | R′G′B′ PQ/BT.2020 10-bit codes as captured (`Rgb10Signal::Direct`) | The operator's declared PQ codes are the source. |
| Windows Grading (WGC FP16) | `R16G16B16A16Float` scRGB | R′G′B′ 10-bit codes after the shared SDR transfer table (`ScrgbSdrTransform::convert_half_pixel`, `convert.rs:1216`), before the matrix | FP16 has no integer truth; the stream's quantisation of it is the truth the user grades against. |
| Windows HDR (WGC FP16, proven HDR target) | same | R′G′B′ 10-bit PQ/BT.2020 codes after primaries conversion, white gain and `scrgb_component_to_pq_code` (`convert.rs:1078`), before the matrix | As above, in PQ. |
| macOS Pier Grading / HDR (SCK `xf44`) | Y′CbCr 4:4:4 10-bit full range (`hosts/macos/src/capture.rs:228`), after the Core Image 203-nit white stage for HDR | Y′CbCr 4:4:4 10-bit codes as captured | ScreenCaptureKit already applied the matrix; the `xf44` codes *are* the source. |
| Linux NvFBC Auto/Speed, any 8-bit zero-copy path | GPU only | **none — never refined** | The device-to-device fast path must not gain a CPU copy (root `AGENTS.md`). |
| Wayland Grading/HDR (PipeWire `RecordMonitor` DMA-BUF, Mutter ≥ 50 forced-HDR route) | Read back once per frame into host memory as packed ten-bit words (`xBGR2101010`; half floats normalised by `convert_rgba_f16_pq_to_packed_rgb10` first), the buffer NVENC's staging converts (`hosts/capenc/src/wayland/pipelines/wide.rs`) | R′G′B′ 10-bit codes after the encoder's own `Rgb10Signal` (`Encoder::wide_signal`): for Grading the composition's PQ/BT.2020 converted to BT.709/sRGB SDR (`PqBt2020ToSdr*`), for HDR the PQ/BT.2020 codes as read back (`Direct`) | The read-back already exists for the encoder; mirroring the very words it stages makes the tile the encoder's input. |

So a tile has one of two domains: **RGB** (R′G′B′ codes, applied after the
Deck's Y′CbCr→RGB step) or **YCbCr** (Y′CbCr 4:4:4 codes, applied before
it, through the same matrix as the video). Both are at the stream's bit
depth (10 in v1; the format reserves 12). *Amended (R1):* the two domains
have separate, precise exactness definitions, and the first slice refines
only RGB tiles of a negotiated 4:4:4 stream.

### 2. When it runs

Per contract, resolved by one shared function
(`arcen_media::video::refine::resolve_refine_plan`, new) from the resolved
plan, the capture backend, Pier configuration and the Deck's capability —
platform code does not decide it:

| Contract | v1 default once qualified | Notes |
| --- | --- | --- |
| Speed | **off, always** | Never touches the eight-bit fast path. |
| Auto | off | Auto is 8-bit and presented through the ordinary SDR path. An eight-bit refinement for CPU-resident Auto captures (WGC BGRA8) is a possible later extension through the egui presenter, never NvFBC. |
| Grading | **on** | The case this exists for. |
| HDR | **on** | RGB domain in PQ/BT.2020, or YCbCr on the macOS Pier. |

Pier configuration `video.lossless_refine` (new, in
`arcen_session::pier_config::VideoConfig`, `shared/session/src/pier_config.rs:133`):
`off` | `auto` (Grading and HDR) | values for `still_ms` and `max_mbps`.
The first release that ships it defaults to **`off`** until the lab
qualification below passes on each Pier; the following release makes `auto`
the default. The Deck can switch it off per connection.

### 3. Scheduling: per block, after stillness, within a budget, interrupted by motion

The host runs a shared `RefineScheduler` (new, `arcen_media::video::refine`)
over Keel's 16×16 grid for each monitor:

1. **Damage schedules, a 128-bit fingerprint decides.** Every captured
   frame updates the block state from Keel (`DamageTracker` where the frame
   is on the CPU, `ExternalDamage` where the OS reports rectangles). The
   scheduler also keeps the XXH3-128 fingerprint of every block it *sent*,
   taken on the refinement source, and recomputes all of them before it
   sends a digest (§5) — 2–4 ms for a 1080p frame on the Deck-class CPU and
   about 14 ms on the Pier-class one, unoptimised, and only once the picture
   has settled. A
   refined block is never trusted on a 64-bit hash or an OS rectangle alone.
2. **Per-block stillness.** A block becomes eligible after it has been
   unchanged for `still_ms` (default 300 ms) *and* the video frame carrying
   its last change has been emitted (so its invalidation has a sequence
   number, §4). Stillness is per block, not per frame: a ticking clock or a
   blinking caret does not stop the rest of the screen from refining.
3. **Global motion gate.** While more than 2% of a monitor's blocks —
   counted **distinct**, a block that changed every frame once — changed in
   the last 250 ms (amended from 5%/100 ms after the first lab measurement,
   see the first-slice report; distinct counting after code review, so a
   1% video region at 60 fps does not stop the rest of the screen)
   (scrolling, playback, a window drag), refinement for
   that monitor pauses entirely, and tiles already queued are cancelled
   (§5). The video encoder owns the link while there is motion.
4. **Order.** Most recently settled blocks first (where the user just
   worked), then raster order.
5. **Budget.** A token bucket per Pier session at
   `min(video.lossless_refine.max_mbps (default 25), the session's current
   video bitrate target)`: a still picture frees the bitrate the link has
   already carried, and refinement never asks for more than that.
   *Superseded by the refinement-budget amendment below: the rate is the
   link's measured residual (R6); the video target is only the floor before
   the first path sample.*
6. **Grouping.** Eligible blocks are grouped into rectangles of at most
   16×4 blocks (256×64 pixels) for compression; single 16×16 tiles compress
   less than half as well (measurements below).

The encoder is not changed in v1. Keel's frame-level gating
(`SubmissionGate`/`IdleCadence`, `shared/keel/src/cadence.rs`) already stops
encoding a still desktop apart from keepalives, and a block that changes is
encoded normally. Biasing the QP map against refined blocks is rejected for
now: inter prediction from a deliberately degraded reference would damage the
next motion that crosses it.

### 4. Ordering: invalidations ride with the video, tiles never apply to stale content

QUIC orders bytes within a stream, never across streams. A tile must never be
painted over content that has since changed, so:

- **`RefineInvalidate`** (new binary record `0x0A`, routed to
  `LaneRole::Video`) carries `monitor_id`, `topology_generation`,
  `stream_epoch`, a per-monitor sequence `seq: u32` and a run-length bitmap
  of the blocks that changed. The Pier writes it on the video lane
  immediately **before** the access unit that encodes that change, so it can
  never arrive after the picture it describes. It is sent when a block
  changes that has had a tile *sent* since the monitor's refinement was last
  cleared — whether or not that tile was delivered — so the Pier never needs
  to know what the Deck applied to keep it from showing stale content.
- **`RefineTiles`** (new `0x0B`, refine lane, §5) carries the same
  `monitor_id`/`topology_generation`/`stream_epoch`, `valid_from_seq` (the
  invalidation sequence current when the source frame was captured), the
  domain and depth, a block rectangle, the codec and the payload.
- The Deck keeps, per block, the sequence of the last invalidation that
  covered it. It **applies invalidations on receipt**, in the video lane
  reader (`video_lane_reader`, `clients/macos/src/transport/lanes.rs:366`),
  before the frame queue — the frame queue may coalesce frames, an
  invalidation must never be dropped. Clearing an overlay early only shows the
  lossy picture of the same content for a frame; clearing it late would show
  wrong content, so early is the safe side.
- A tile applies to a block only if no invalidation newer than
  `valid_from_seq` covered it. A tile from the future (its `valid_from_seq`
  not yet seen on the video lane) waits in a bounded pending set (8 MiB per
  session) until the video lane catches up; on overflow the Deck drops it and
  asks for a resend (§7).
- A different `topology_generation` or `stream_epoch`, a lost video or
  refine lane, or a Deck decoder teardown clears that monitor's refinement on
  both ends. The Pier's own cancellation of the refine lane (§5) does not: what
  the Deck already applied stays valid under the rules above, and the Pier
  sends the cancelled blocks again later. A keyframe does **not**: it re-encodes the same content, and the
  overlay is about content, not about the decoder. *Amended (R2, R3):* the
  sequence becomes a per-monitor source fence carried before every access
  unit, and a **recovery** keyframe (decoder reset, loss) does clear: it
  starts a new refinement generation behind an ordered reset barrier.

### 5. A refine lane, below everything else

Tiles travel on a new lane role, **`refine`** (unidirectional, Pier → Deck,
`u32` BE length + binary record like `video`, send priority **−20**, below
`bulk_aux`'s −10; ADR 0014 §1).

- Not the video lane: a megabyte of tiles queued there would delay the first
  frame of the next motion (head-of-line blocking).
- Not `bulk_aux`: that lane carries HID reports, USB URBs and clipboard
  chunks, which would queue behind tiles.
- **Motion cancels queued tiles.** When the motion gate closes and more than
  64 KiB of tiles are still queued, the Pier resets the refine stream and
  opens the next lane generation (ADR 0014 supersession), discarding the
  stale bytes instead of sending them. Records already delivered are still
  safe: §4 drops any that motion invalidated.
- The lane is opened only when the Deck's `ClientHello` carried the
  capability (§8), so an older Deck's lane registry never sees an unknown
  role. The Deck's uni-stream grant
  (`arcen_transport::lanes::DECK_LANE_STREAM_LIMITS`,
  `shared/transport/src/lanes.rs:129`) grows from 3 to 4. *Amended (R7):*
  the `refine` role appears in `lane_attachment.roles` only for a Deck that
  advertised the capability, because an older Deck's grant validator refuses
  an unknown role token and a role list as long as its own table.

`RefineDigest` (new `0x0C`, refine lane) states what is exact. Whenever a
monitor's refinement settles — no tile queued and none due for 500 ms — the
Pier sends `(monitor_id, topology_generation, stream_epoch, seq, coverage,
digest)`: `coverage` is a run-length bitmap of the refined blocks and
`digest` is XXH3-128 over their XXH3-128 fingerprints in raster order, each
fingerprint taken over the block's planar little-endian `u16` codes (plane 0,
1, 2; the block's own rows only). The Deck computes the same value from the
tiles it applied (§7). Equal digests mean the overlay holds exactly the
source's code values for every covered block. Coverage need not be the whole
screen: a ticking clock or a blinking caret stays live, and the verdict says
so instead of never arriving.

### 6. Tile format: left-neighbour residuals, byte-split, Zstandard

Measured on this design's own harness with seven 1920×1080 ten-bit frames:
three real desktops at depth 30 (8-bit UI as the X server expands it, by bit
replication), a 10-bit SDR photograph, the same photograph converted to
PQ/BT.2020, that PQ photograph with film-like grain (σ = 2 codes), and 10-bit
gradients. Ratio is against packed 30-bit RGB (7.78 MB a frame); encode
speed includes the predictor, decode speed is decompression only; both are
the slowest frame, one core, on an Apple M4 Pro (the Deck class) and on the
lab's AMD EPYC 7742 virtual machine (the Pier class, five of the seven
frames, other builds running). The harness and raw results are kept with the
design review evidence, not in the repository.

| Tile 256×64, RGB | UI (×3) | 10-bit gradient | photo SDR | photo PQ | PQ + grain | geo. mean | encode Mpx/s M4 / EPYC | decode Mpx/s M4 / EPYC |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **left + byte-split + zstd 1** | 10.0–131 | 28.4 | 2.64 | 2.44 | 2.05 | **10.9** | 121 / 44 | 358 / 148 |
| median-edge (LOCO-I) + byte-split + zstd 1 | 9.1–142 | 30.3 | 2.63 | 2.44 | 2.07 | 11.1 | 93 / 17 | 366 / 149 |
| left + byte-split + deflate 1 (pure Rust) | 4.6–40 | 24.3 | 2.34 | 2.26 | 1.96 | 7.0 | 68 / 22 | 104 / 47 |
| left + byte-split + LZ4 (pure Rust) | 3.2–60 | 15.0 | 1.58 | 1.60 | 1.27 | 5.7 | 148 / 59 | 259 / 148 |
| packed RGB10 + zstd 1 | 11.5–103 | 3.8 | 1.68 | 1.63 | 1.24 | 6.4 | 137 / 45 | 330 / 120 |
| left + byte-split + deflate 6 | 8.2–68 | 58.3 | 2.49 | 2.34 | 1.89 | 9.8 | 8 / 3 | 109 / 46 |
| left + byte-split + zstd 1, 16×16 tiles | 3.6–23 | 6.4 | 2.20 | 2.09 | 1.80 | 4.8 | 41 / 14 | 103 / 32 |
| median-edge + byte-split + zstd 1, Y′CbCr 4:4:4 | 9.5–143 | 9.6 | 2.45 | 3.37 | 2.59 | 10.0 | 86 / 15 | 312 / 120 |

Decision:

- **Codec v1 = `delta-zstd1`:** per plane, the residual against the left
  neighbour (the pixel above for a row's first pixel, zero for the first),
  modulo 2^depth and folded to an unsigned value (zigzag), split into a
  low-byte plane and a high-byte plane, then one Zstandard frame at level 1
  for the whole rectangle. The median-edge predictor compresses within 2% of
  it and is 2.6× slower on the Pier-class CPU; Zstandard level 3 and deflate
  level 6 do not improve the geometric mean and cost up to fifteen times the
  CPU. Single 16×16 tiles compress less than half as well as 256×64
  rectangles.
- **Same codec in both domains.** Y′CbCr compresses photographs better and
  gradients worse; the domain is fixed by §1, not chosen for ratio.
- **Native dependency, feature-gated.** Zstandard needs a C build, which the
  default `arcen-media` graph forbids. The codec lives behind a new
  `arcen-media` feature `lossless-refine` (like `software-h264-source`),
  enabled by capenc, the macOS Pier and the Deck; the default-graph purity
  proof is unchanged, and the library's licence notice joins
  `legal/THIRD_PARTY_NOTICES.md`. The codec id is negotiated, so a pure-Rust
  fallback (`delta-deflate1`, 56% more bytes) can be added without a wire
  change.
- **What it costs.** A full 1080p frame of 10-bit photographic content is
  about 3.0–3.8 MB (1.0–1.2 s at 25 Mbit/s); desktop UI is 0.06–0.8 MB
  (under 0.3 s). 4K is four times that. Encoding a whole 1080p frame costs
  about 50 ms of one Pier-class core, spent only while the picture is still
  and never faster than the budget drains. Grain is the worst case and the
  honest one: lossless means sending the grain.

### 7. Deck compositing and the exactness report

- *Amended (R1, R2, R4):* exactness is scoped to source codes before display
  transforms, uploads are fenced and Metal-synchronised, and verification has
  three levels.
- **Metal, inside the conversion pass, before display transforms.** The
  dedicated layer gains one texture per monitor, `RGB10A2Uint` at the
  source's size (33 MB at 4K): three 10-bit code channels plus a 2-bit
  "valid" flag. `fs_convert` reads it at the luma coordinate. If valid, an
  RGB-domain tile writes `code / 1023` directly (the drawable is
  `RGB10A2Unorm`, so the stored code is the tile's code), and a Y′CbCr tile
  replaces `luma_code`/`cb_code`/`cr_code` before the matrix — no chroma
  reconstruction for a refined pixel. PQ tagging and EDR, which Core
  Animation applies after the pass, are unchanged. The same change goes into
  `video_render.wgsl` only if refinement is ever extended to the egui path.
- **Uploads only on change.** A tile writes its rectangle with
  `replaceRegion`; an invalidation clears its blocks' valid bits. When the
  overlay changes and no new video frame is due, the presenter re-presents
  its retained frame (`last_frame`) so a still screen visibly sharpens.
- **CPU reference and digest.** The Deck decodes each tile on a refine
  worker of its own (never the video decode thread), computes the block
  fingerprints from the decoded codes, keeps them (16 bytes a block) and, on
  `RefineDigest`, computes the digest of §5 over the stated coverage. It needs no CPU copy of the picture. A diagnostic mode reads the
  overlay texture back and re-fingerprints it, which proves the GPU holds what
  the CPU decoded.
- **Visible to the user.** The status line shows, per monitor, *Refining
  NN%* while tiles arrive and **Lossless ✓ bit-exact** once the digest
  matched — with the share of the screen it covers when some blocks are live
  ("bit-exact, 99.7%; 3 live blocks") — cleared by the next invalidation. A
  diagnostic overlay tints blocks (exact / live / pending), so the region
  that is exact is visible, not only a number. The colour check report
  records the verdict — domain, depth, transfer, primaries, block count,
  digest — so a colourist's report says "the picture you judged was the
  source, code for code", not "within one code on most pixels".
- A digest mismatch is a defect, not a state: the Deck logs it, drops the
  overlay for that monitor and sends `refine_reset`.

### 8. Negotiation and plan truth

All additive and serde-default; `PROTOCOL_VERSION` stays 5.

- `ClientHelloMsg.lossless_refine_v1` (new): domains and codecs the Deck
  can apply (`["rgb","ycbcr"]`, `["delta-zstd1"]`). Absent on older Decks:
  nothing is sent.
- `ServerHelloMsg.lossless_refine` (new, next to `active_pipeline`,
  `shared/protocol/src/messages.rs:2378`): `state` (`on`, `off`,
  `unavailable`), `reason` when not on (`speed_contract`, `eight_bit`,
  `zero_copy_capture`, `deck_unsupported`, `deck_eight_bit_fallback`,
  `operator_off`, …), and, when on,
  `domain`, `bit_depth`, `transfer`, `primaries`, `matrix` (Y′CbCr only),
  `range`, `codec`, `still_ms`, `max_mbps`. The Deck shows the reason, never
  guesses.
- capenc's READY line appends `refine=rgb10|ycbcr444-10|off`. Because
  `parse_ready_v1` rejects unknown fields
  (`shared/media/src/video/plan.rs:1140`), the parser learns the field in
  the same change as the producer; capenc and the Pier ship together.
- capenc framed-v2 (`arcen_media::capenc_stream::CapencRecordKind`,
  `shared/media/src/capenc_stream.rs:34`) gains kinds 3 (invalidation, which
  the Pier writes on the video lane before the next access unit) and 4/5
  (tiles, digest, routed to the refine lane). The Pier bounds each record
  before allocating, as for cursor records.
- Deck → Pier `refine_reset {monitor_id, stream_epoch, reason}` on the
  control lane: the Pier forgets what it sent for that monitor and refines
  it again. It is how pending-set overflow, a Deck-side teardown and a digest
  mismatch heal.

### 9. Failure modes

| Failure | Handling |
| --- | --- |
| A tile arrives after its blocks changed | Dropped by the per-block sequence check (§4). |
| A tile arrives before its invalidation sequence | Held, bounded; overflow → drop + `refine_reset`. |
| Invalidation lost with a video lane reset | Lane reopen clears the monitor's overlay on the Deck and the sent-state on the Pier. |
| 64-bit hash collision hides a change | Sent blocks are re-verified with XXH3-128 on the source (§3); the digest would also disagree. |
| Pier refine queue full | Pier drops the record and tells capenc through the existing control channel; capenc sends those blocks again later and, as for any sent block, still invalidates them on change. |
| Geometry, topology or epoch change | Both ends clear the monitor. |
| Decoder teardown / reconnect | Deck clears and sends `refine_reset`; reconnect starts with nothing refined. |
| Native 10-bit layer fails (8-bit fallback) | Refinement stops (`state=unavailable reason=deck_eight_bit_fallback`); the 8-bit path is never fed tiles. |
| Link saturated | Budget is the link's measured residual, suspended under congestion, loss, a video backlog or rising frame age, and paused for one window before queueing is blamed on refinement (refinement-budget amendment); the refine lane has the lowest priority; motion cancels the queue. |
| Old Deck / old Pier | Capability absent → no lane, no records. |

### 10. Telemetry

Shared events (vocabulary in `arcen-telemetry`, emitted through
`arcen-observability`): `video.refine.plan` (state, reason, domain, codec,
budget), and a window summary `video.refine.window` on both ends — tiles,
blocks, raw and wire bytes, ratio, coverage per monitor, time from stillness
to exact, invalidations, stale drops, pending overflows, lane cancellations,
digest matches and mismatches. The Deck's session telemetry carries the
current coverage and the last verdict.

### 11. Tests

- Shared: codec round trip on edge rectangles (partial blocks at right and
  bottom edges), domains and depths; a corrupted payload is refused before
  allocation; scheduler property tests (no tile for a block changed after
  `valid_from_seq`; motion gate; budget never exceeded over any window);
  Deck-side state machine (out-of-order tiles, future tiles, overflow, epoch
  change); digest equal on both ends for a test pattern, for a partial coverage
  bitmap, and different after a one-code change in one pixel.
- Wire: encode/decode of `0x0A`–`0x0C`, routing to `video`/`refine`,
  `WIRE.md` vectors, old peers ignore the absent capability.
- Deck: a Metal test renders a frame with an RGB and a Y′CbCr overlay through
  the production `fs_convert` and checks the read-back codes are exactly the
  tile's (as `video_metal_layer::shader_tests` already does for planes).
- Lab, per Pier and contract: a still `arcen_media::test_pattern` frame,
  then motion, then stillness again; pass = digest match within
  `still_ms + 2 s` at 1080p, overlay cleared on the first motion frame, no
  change in Speed's frame timing with refinement enabled, and a real-screen
  screenshot of the indicator.

### 12. Work list

1. **Shared** (`arcen-protocol`, `arcen-media`, `arcen-transport`,
   `arcen-telemetry`, `arcen-session`): records and capability, refine lane
   role, `resolve_refine_plan`, scheduler, codec, digest, Deck-side state,
   config key, telemetry; consumers registered in each `consumers.json`.
2. **Linux Xorg Grading** (`hosts/capenc/src/linux.rs` `run_wide_encode`,
   `:1667`): feed the CPU frame it already has to the scheduler; write
   framed-v2 records; the Pier routes them. The NvFBC path is untouched.
3. **macOS Deck**: lane reader for `refine`, invalidations in the video lane
   reader, tile decode on a refine worker, overlay texture and `fs_convert`
   change, re-present on overlay change, indicator, colour check report field.
4. **Windows Grading/HDR** (`hosts/capenc/src/nvenc.rs` around the CPU
   scRGB conversions at `:975`): expose the R′G′B′ code stage as its own
   shared function so refinement and encoding share it.
5. **macOS Pier**: done 2026-11-13, Y′CbCr domain, over a whole-surface
   compare of every encoded `xf44` surface (see the macOS amendments).
6. **Wayland** Grading/HDR: done 2026-11-13 over the existing whole-frame
   host read-back (the eligible-blocks-only GPU read-back stays a possible
   optimisation).

## Review 2026-11-09

A design review found seven blocking issues. Each is resolved below; these
rules are normative and replace any conflicting sentence above. Shared names
refer to the first-slice code in `arcen_media::refine` (portable, codec-free)
and `arcen_protocol::refine` (wire records).

### R1. Exactness is defined per tile domain

"Bit-exact" is a claim about **source code values**, never about a decoded or
displayed picture in general. The two domains have different claims:

- **RGB tiles** carry full-range, transfer-coded R′G′B′ codes at the stream's
  bit depth, in the contract's transfer and primaries. The presenter writes
  them **directly** into the source-resolution composite: for a valid overlay
  texel the conversion pass outputs `code / (2^depth − 1)` per channel into
  the `RGB10A2Unorm` target and skips the Y′CbCr→RGB matrix entirely. The
  claim is *source fidelity*: the composite holds the source's own R′G′B′
  codes for every covered pixel. It is not a claim that video decoding was
  perfect; for covered pixels the decoded video is simply not used.
- **Y′CbCr tiles** (macOS Pier `xf44`, since 2026-11-13) replace the decoded
  `luma_code`/`cb_code`/`cr_code` before the matrix and then use **exactly**
  the presenter's own range and matrix uniforms (`VideoUniform::from_contract`
  — the same `luma_offset`, spans and `chroma_center`, 512 at 10 bits) that
  the video uses. Their claim is "the presenter's input codes equal the
  source's Y′CbCr codes"; the RGB output is then the presenter's documented
  function of them.
- **First slice:** RGB domain only, and only on a stream that negotiated
  4:4:4. A 4:2:0/4:2:2 stream, a Y′CbCr source or an 8-bit stream is refused
  by `resolve_refine_plan` with a reason.

### R2. Presentation fences and one state owner

- Each monitor has a **source fence**: a `u64` the Pier assigns to every
  access unit it emits while refinement is granted, strictly increasing within
  a refinement generation. A `RefineFence` record (video lane) immediately
  precedes **every** such access unit and carries the fence and the blocks
  invalidated by that access unit (possibly none).
- **Damage belongs to the encoder output that carries it.** Damage from
  captured frames that were dropped or coalesced before encoding is unioned
  into the next emitted access unit's invalidation; it is never discarded.
  On the Deck, invalidations are applied on receipt in the video lane reader
  (never through the coalescing frame queue), and per block only the newest
  invalidating fence is kept — so dropped or coalesced decoded frames cannot
  lose an invalidation.
- The decoded frame carries its fence to the presenter, which reports the
  **presented fence** separately from the **received fence**. A tile captured
  at fence *t* becomes visible only when `presented ≥ t` and no received
  invalidation with fence `> t` covers its block. Clearing on receipt is the
  safe side (the lossy picture of the same content shows for a frame).
- **One state owner.** The Deck's per-monitor `DeckRefineState` (shared, pure)
  is the only place that decides validity; the lane reader, the decode worker
  and the presenter submit events to it and read decisions from it.
- **The Pier's video queue never loses a fence.** The Linux Pier's send
  queue drops an access unit under pressure and then suppresses non-key
  access units until a keyframe (`enqueue_classified`,
  `hosts/linux/src/session/client.rs`). A fence attached to a dropped or
  suppressed access unit must not be dropped with it: the queue keeps the
  union of their invalidations (and the newest fence) and writes it before
  the next access unit it does send. The recovery keyframe that follows
  also starts a new generation (R3), but without this rule a tile of the old
  generation could be shown over changed content until that keyframe
  arrives. This is the first change the Pier needs before it forwards
  refinement.
- **Revalidate at commit.** Decompression happens off the owner; a decoded
  tile is re-checked against the owner (generation, fences, invalidations)
  immediately before its GPU commit, because an invalidation may have arrived
  while it was being decompressed.
- **Metal-synchronised uploads.** Overlay writes are encoded into the
  presenter's own command buffer (blit from a staging buffer) before the
  conversion pass that samples them, never by CPU `replaceRegion` into a
  texture a command buffer in flight may be reading.

### R3. Refinement generations and the ordered reset barrier

- Every tile, digest, decode job and commit carries a `refine_generation`
  (`u32`, nonzero). The Pier starts a generation with a `RefineBarrier`
  record on the **video lane** before the first access unit of that
  generation; the Deck adopts a generation only from the barrier, in video
  order. Tiles or digests of an older generation are discarded; of a newer
  one not yet seen on the video lane are held (bounded) or dropped.
- Both ends start a new generation (and the Deck clears the monitor's
  overlay) on: stream discontinuity (new `stream_epoch`), decoder recovery or
  reconnect, resize or any geometry change, a colour-contract change
  (domain, depth, transfer, primaries, matrix, range, chroma), topology
  change, refine-lane loss, Deck re-enable after a disable, and
  `refine_reset`.
- **Recovery keyframes do not inherit validity.** A keyframe the Deck asked
  for after loss, or after a decoder reset, is preceded by a barrier; a
  routine periodic keyframe is not, and keeps the overlay (same content).
- **Fail safe (code review 2026-11-09).** A barrier on the ordered video
  lane is authoritative: the Deck starts over on *every* barrier, whatever
  generation it names — newer, older or identical — because nothing
  reorders that lane and a restarted producer may repeat an identity. A
  fence whose identity (monitor, topology generation, epoch, generation)
  differs from the current generation, an out-of-order fence, or a fence
  for another grid clears the monitor's overlay and sends `refine_reset`.
  A producer starts each process from a per-process generation
  (`arcen_media::refine::producer::process_unique_generation`), so a
  restarted capenc does not repeat one. A barrier that reuses the current
  identity with another source size is refused and clears the monitor: a
  generation id is never reused for other geometry.
- **Keepalives are not content.** A keepalive access unit advances the
  fence and nothing else: it schedules no tile and triggers no digest. A
  digest is due only when the covered set changed; a producer whose damage
  is exact (the compare-and-copy mirror) digests the fingerprints taken at
  extraction and never re-reads the screen on its capture thread.

### R4. Three verification levels and a canonical digest

| Level | What is compared | Published after |
| --- | --- | --- |
| 1 — source codes | Codes the Deck decompressed (CPU) vs the Pier's digest of the source | the digest's fence has been **presented** and every covered block is committed and still valid |
| 2 — GPU overlay | Overlay texture read back vs the same digest | the command buffer that wrote the overlay completed |
| 3 — rendered RGB | Rendered composite read back (RGB domain, unscaled offscreen target) vs the tile codes | the frame carrying that fence was rendered and its command buffer completed |

Wording is scoped and exact: "**Source codes verified (pre-display
transforms); 99.7 % coverage; level 1**". Never "lossless" without the level
and coverage. A digest superseded by a newer one, by an invalidation of a
covered block or by a new generation is discarded, never published late.

**Canonical digest v1** (`arcen_media::refine::digest`): every integer
little-endian, no padding.

```
block_fp = XXH3-128( "arcen.refine.block.v1\0"
                     || u16 width || u16 height        // clipped to the source
                     || plane0 samples || plane1 samples || plane2 samples )
           // R,G,B (or Y,Cb,Cr), row-major, each sample a u16 LE with the
           // code right-aligned (bits above depth zero); no alpha, no cursor
digest   = XXH3-128( "arcen.refine.digest.v1\0"
                     || u16 monitor_id || u64 topology_generation
                     || u64 stream_epoch || u32 refine_generation
                     || u64 fence
                     || contract: u8 domain, depth, transfer, primaries,
                                  matrix, range, chroma
                     || u32 source_width || u32 source_height
                     || u16 block_size (16)
                     || u32 covered_count
                     || for each covered block, ascending raster index:
                          u32 block_index || block_fp (16 bytes, LE u128) )
```

The coverage bitmap travels with the digest; the digest commits to it via
the indices. The cursor is never part of a tile or a digest (R8).

### R5. Normative limits, enforced before allocation

| Limit | Value |
| --- | --- |
| Tile rectangle | aligned to the 16-pixel grid; ≤ 256 × 64 pixels; clipped only at the source's right/bottom edge |
| Expanded size | `w × h × 3 × 2` bytes, computed with checked arithmetic from the header, **≤ 98,304 B** |
| Compressed record payload | ≤ 131,072 B (128 KiB) |
| Payload framing | exactly one Zstandard frame; `Frame_Content_Size` present and equal to the expected size; window ≤ 128 KiB; no dictionary ID; no skippable frames; no trailing bytes |
| Enforcement point | header and frame-header checks happen before any decoder allocation |
| Invalidation bitmap | run-length over the monitor's block count; total runs ≤ block count; decoded length must equal the block count |
| Deck pending (received, not committed) | ≤ 8 MiB decoded bytes per session, ≤ 256 jobs; a job older than 2 s is dropped |
| Deck GPU staging | ≤ 4 MiB per presented frame; the rest waits for the next frame |
| Deck rate | ≤ 4,000 tiles/s accepted per session; excess is dropped |
| Failure disable | 8 malformed records within 10 s disable refinement for the session, visibly (`disabled: malformed_records`) |
| Monitors | at most 16 refined monitors per session (`MAX_REFINE_MONITORS`); a barrier for a 17th is refused |
| Session blocks | all refined monitors together hold at most 2^20 blocks (`MAX_SESSION_REFINE_BLOCKS`, one 16384×16384 monitor); a barrier beyond it is refused and clears that monitor |
| capenc output | `framed-v2` is accepted only by paths that write it (Xorg depth-30, Wayland) and `refine=on` only by those paths (`CapencRoute::honours_refinement`; on Wayland only the wide composition refines, the eight-bit and software ones log why not); both are refused at argument parsing elsewhere, and with `framed-v2` every access unit is a framed-v2 record even when refinement is refused |

### R6. Budget: measured residual, never in front of video

- **Residual.** The Pier measures, over the last window, the path's
  delivered rate and the interactive traffic (video, audio, input, cursor).
  Refinement may use `min(max_mbps, session_cap, ½ × (delivered −
  interactive))`, never negative; the cap is session-wide across monitors.
- **Token bucket invariant.** Over any interval of length *W*, refine bytes
  written ≤ `rate × W + burst`, with `burst ≤ 256 KiB`; refine bytes written
  but not yet acknowledged by QUIC ≤ 512 KiB.
- **Suspension.** Refinement is suspended (bucket frozen, queue cancelled)
  while a congestion event happened in the last second, QUIC reports loss
  above 1 %, the video lane has a backlog, or Grading frame age rises; it
  resumes after one quiet second.
- **Never blocks capenc.** Tile extraction copies eligible rectangles into
  an immutable snapshot; compression runs on a separate worker; finished
  records go through a bounded queue that the ordered stdout/video writer
  drains with a non-blocking poll between access units. A full queue drops
  refinement work, never video.

### R7. Capability gating and per-monitor readiness

- `lane_attachment.roles` lists `refine` only when the Deck's hello
  advertised `lossless_refine_v1` with a codec the Pier will use and the
  Pier's plan is on. An older Deck therefore never receives the token
  (`LaneAttachmentMsg::validate` refuses unknown tokens and over-long lists).
- `RefineFence` and `RefineBarrier` records (video lane) are likewise sent
  only to a Deck whose grant included `refine`; an older Deck never sees an
  unknown video-lane frame type.
- Readiness is per monitor: the Pier announces `on`/`off`/`unavailable` with a
  reason per monitor; the Deck may disable one monitor (`refine_control`).
- If the Deck's 10-bit layer falls back to 8 bits, the Deck disables
  refinement for that monitor; the Pier stops sending tiles for it. A later
  re-enable starts a new generation.

### R8. Non-blocking amendments

- **Codec split.** The portable state, limits, tile layout, predictor and
  digest are codec-free. The Zstandard **encoder** (native `zstd`) is a Pier
  feature; the **decoder** is a separate Deck feature and was chosen by
  benchmark: a pure-Rust decoder is preferred when its speed suffices,
  because it keeps the Deck free of a native build. Measured on the M4 Pro
  (256×64 tiles, decompression plus the residual transform): `ruzstd` 151
  and native `zstd` 166 Mpx/s on UI content; 32 and 116 Mpx/s on
  photographic content with grain. 32 Mpx/s decodes a full 1080p
  photographic frame in about 65 ms of one worker core, which a 25 Mbit/s
  budget delivers over about a second, so the Deck uses `ruzstd`; native
  decoding stays a drop-in if budgets grow.
- **Coordinates.** Tile and block coordinates are visible source pixels of
  the captured monitor (before any Deck scaling); the Deck composites in
  source space, then scales.
- **Cursor.** The cursor is outside tiles and digests. Where a Pier bakes the
  cursor into captured pixels, a cursor move damages both the old and the
  new footprint, and blocks under the cursor are live, not refined. Xorg
  XShm capture contains no cursor.

### Minimal first slice

| Part | Where | State (2026-11-13) |
| --- | --- | --- |
| Plan, limits, tile layout, residual transform, canonical digest, Pier scheduler (generations, fences, damage union, motion gate, busy-block back-off, token bucket), Deck state owner (fences, generations, invalidation union, revalidation, verification levels, scoped status) | `shared/media/src/refine/` (codec-free) | implemented, tested |
| Exact source mirror and producer behind the `RefineSource` seam (packed RGB10 today; eight-bit proven in tests); capenc runtime with one compression worker, rate from the link's residual (refinement-budget amendment; the video bitrate is the floor before a path sample) | `arcen_media::refine::{source, producer, runtime}` | implemented, tested |
| One shared entry point: `plan_for_capture`, hello status, capenc arguments, off-reason wording; forwarding carry; Deck hub with the `RefineCompositor` seam | `arcen_media::refine::{mod, carry, hub}` | implemented, tested |
| Wire records `RefineFence` 0x0A, `RefineTiles` 0x0B, `RefineDigest` 0x0C, `RefineBarrier` 0x0D; `refine` lane role; capability DTO; gated grant | `shared/protocol` | implemented; live between a Linux Pier and the macOS Deck |
| Zstandard encoder (Pier, native `zstd`) / decoder (Deck, pure-Rust `ruzstd`) | `arcen-media` features `refine-zstd-encode` / `refine-zstd-decode` | implemented, tested |
| Linux Xorg depth-30 `XShm` Grading producer, one monitor | `hosts/capenc` (default feature `lossless-refine`) + `refine=on framed-v2` | implemented; live |
| Linux Wayland Grading and HDR producer (wide read-back), one monitor | `hosts/capenc/src/wayland/pipelines/wide.rs` | implemented; live (2026-11-13) |
| Y′CbCr 4:4:4 source (planar or biplanar, 8–12 bits), Y′CbCr plan, contract matrix, Deck domain guard | `arcen_media::refine::{ycbcr, mod}` | implemented, tested (2026-11-13) |
| macOS Pier Grading and HDR producer (`xf44`, Y′CbCr), one monitor | `hosts/macos/src/refine.rs` (default feature `lossless-refine`) | implemented (2026-11-13) |
| Metal Y′CbCr composite (`fs_convert_refined_ycbcr`) | `clients/macos` | implemented, shader-tested (2026-11-13) |
| Pier negotiation and forwarding (capability, grant, capenc arguments, video/refine lane routing, `refine_reset`/`refine_control`) | `hosts/linux` | implemented; live |
| Deck wiring (capability, refine lane reader, fences before the frame queue, decode worker, presented fences) | `clients/macos` | implemented; live |
| Metal overlay, verification levels 2/3 | `clients/macos` (`ui/refine_overlay.rs`) | implemented; live to level 3 |
| Stream Statistics ("Exact: N% of picture verified") and Lossless Tile View | `clients/macos`, `arcen_media::stats_overlay` | implemented; live |

Release builds compile all of it; the three run-time switches are the only
switches. The Deck's uni-stream grant is now 4 (video, audio, refine and one
replacement), which ADR 0014's table predates.

### First-slice amendments (2026-11-13)

Made while wiring the products; each is normative.

1. **The capability travels in `AuthResponse`, not `ClientHello`.** A Pier
   starts its encoder, grants lanes and sends its hello before it reads the
   Deck's hello, so the Deck's capability must arrive at authentication.
   The Deck sends it only while its setting is on.
2. **One monitor.** The Deck's state owner admits one monitor; a plan for
   several monitors is off (`multi_monitor`), so the scope is constant.
3. **A dropped access unit never loses a fence.** When the Pier's send queue
   drops or suppresses access units, their records are merged and written
   before the next access unit it sends: the newest barrier and one fence
   carrying the union of invalidations under the newest fence number
   (`arcen_media::refine::carry`).
4. **The Deck's refine lane applies backpressure.** When the decode backlog
   is full the reader stops reading (≤ 2 s) instead of dropping tiles; QUIC
   flow control holds the Pier back. Tiles that are still dropped wait for
   the digest timeout, which starts a new generation.
5. **Barriers are rate-limited**: more than 16 in 10 s stops refinement for
   the connection (`TooManyBarriers`).
6. **The Deck's overlay is the decoded picture's size**, never a barrier's
   claim; every upload and readback rectangle is bounds-checked, and a tile
   is accepted only for the grid it was decoded for.
7. **Rate cap.** The refinement token bucket follows the encoder's current
   average bitrate (capenc `BITRATE`), so refinement never exceeds the video
   budget the path allows. *Superseded by the refinement-budget amendment:
   the video target is the floor only until the first path sample.*
8. **Defaults.** The Pier ships `disabled` until field evidence supports
   turning it on; the Deck setting is on, so an operator's switch is the
   single decision.

### Wayland amendments (2026-11-13)

Made when the Linux Wayland Grading and HDR source was added; each is
normative.

1. **The source is the encoder's own input.** The wide composition already
   reads each `RecordMonitor` frame back into host memory for NVENC's
   staging; the refinement source mirrors those very packed words and maps
   them with the encoder's own pre-matrix `Rgb10Signal`
   (`PackedRgb10Source`, `Encoder::wide_signal`). Half-float frames are
   normalised once by the shared conversion before either reads them. The
   compositor's damage regions are not trusted: every imported frame is
   compared whole against the mirror, so damage stays exact.
2. **Each contract refines in its own coding domain.** Grading tiles carry
   the BT.709 or sRGB SDR codes the session serves; HDR tiles carry PQ /
   BT.2020 codes. The barrier's contract bytes (H.273 transfer 1/13/16,
   primaries 1/9) and the canonical digest commit to the domain; no record
   or digest format changed.
3. **The Deck composites only in the picture's own domain.** A generation
   is painted only when `RefineContract::describes_stream` holds for the
   decoded picture's depth, transfer and primaries: PQ codes never land on
   an SDR picture, and SDR codes never on a PQ picture under EDR. The
   overlay stores codes, not light, into the same `RGB10A2Unorm` target the
   PQ/EDR layer presents, so verification is unchanged.
4. **The Wayland route accepts `refine=on`**
   (`CapencRoute::honours_refinement`); only its wide composition refines.
   The eight-bit and software compositions keep their paths untouched and
   log `eight_bit`. A Linux Pier decides the source kind from the depth
   alone (`RefineSourceKind::for_linux_capture`): every ten-bit Linux capture
   is staged from host memory, every eight-bit one stays on the GPU.
5. **A word-order change ends the epoch** instead of mirroring the wrong
   layout; the attachment restarts it, and the new capenc starts a new
   generation.

### macOS amendments (2026-11-13)

Made when the macOS Pier's Grading and HDR source was added; each is
normative.

1. **The source is the surface VideoToolbox encodes.** Grading and HDR are
   captured as `ScreenCaptureKit` `xf44` (ten-bit Y′CbCr 4:4:4, full range,
   two planes, codes at the top of 16-bit samples); HDR is re-rendered by
   the Core Image white stage into a fresh `xf44` surface first. That
   encoded surface is the refinement source, refined in the **Y′CbCr**
   domain: its codes are the encoder's input, so there is no conversion to
   be exact about (`arcen_media::refine::ycbcr::YCbCr444Source`, generic
   over planar and biplanar 8–12-bit layouts). A surface that is not that
   `xf44` picture (another format or size) invalidates every block and
   stops tiles for the session.
2. **Damage is exact, never `ScreenCaptureKit`'s.** Every encoded surface
   is compared whole with the source's mirror (rows first, then the blocks
   of a changed row). The compare runs on a worker thread while
   VideoToolbox encodes the same surface; the access unit waits only for
   the fence naming what changed in it. Dirty rectangles keep their
   existing role in frame scheduling and are not used here. Tiles,
   compression and the digest run on the worker, bounded by the token
   bucket (capped at the encoder's target; since the refinement-budget
   amendment, at the link's residual) and by bounded queues; nothing
   of it runs on the capture or encode thread.
3. **The contract carries the matrix and range.** A Y′CbCr plan comes from
   `RefineSourceKind::CpuYCbCr444 { matrix, range }` (the matrix the
   encoder tags: BT.709 for Grading, BT.2020 NCL for HDR; `xf44`'s full
   range); the barrier's `matrix` byte is its H.273 value (0 for RGB). A
   Y′CbCr source without a matrix is refused (`ycbcr_source_not_yet`). The
   Deck composites a Y′CbCr generation only when
   `RefineContract::describes_coded_stream` holds — depth, transfer and
   primaries as for RGB, plus the decoded picture's matrix, range and
   4:4:4.
4. **Composite and verification.** `fs_convert_refined_ycbcr` replaces the
   decoded Y′, Cb, Cr codes with the tile's and then runs exactly the
   video's range and matrix step (one shared `codes_to_rgb`), SDR and
   PQ/EDR alike; a shader test proves a refined pixel equals the
   presenter's conversion of a decoded pixel holding the same codes, bit
   for bit. Verification stops at **level 2** (the GPU overlay holds the
   codes): the rendered pixel is the matrix of a code, not the code, so
   level 3 is defined for RGB tiles only.
5. **Negotiation.** The Deck advertises `ycbcr` beside `rgb`; a Deck that
   lists only `rgb` gets `deck_unsupported`. The macOS daemon's relay grants
   the `refine` lane to a Deck whose `auth_response` asked for `ycbcr` when
   the build and `video.lossless_refinement` allow it (it reads only the
   capability, never the credentials beside it); the agent's hello states
   the shared plan, and `refine_reset`/`refine_control` reach the worker
   through a bounded channel.
6. **Measured** (an Apple M2 Ultra host, a macOS Deck over a wide-area link,
   25 Mbit/s ceiling): the worker's compare costs 2–3 ms per encoded
   1080p-class frame and 7–10 ms (worst 20 ms) per 3600×2262 frame, off the
   encode thread; motion frame rate is unchanged (29.2 vs 29.5 fps at
   1800×1130, 21.5 vs 21.6 fps at 3600×2262, on vs off); a 2–3 MB still
   picture is verified in about 6 s at 1800×1130 and 7 s at 3600×2262, and
   about 6 s after motion stops; every run had 0 digest mismatches.

### Refinement-budget amendments (2026-11-13)

Made after the first wide-area measurements; each is normative and
supersedes §3 item 5, the "Link saturated" row of §9 and first-slice
amendment 7.

The coupling they remove: refinement was capped at the video's own bitrate
target, the video's rate controller read the queueing that refinement
bursts add as congestion and cut its target, and the refinement cap
followed it down (measured over a VPN path: 4.7 → 2.3 → 1.2 Mbit/s, and a
4K HDR photograph took 72 s). Each fed the other, and the next motion
started from the collapsed target.

1. **The rate is the link's residual (R6 as written).** Once a second the
   Pier feeds the shared sampler
   (`arcen_media::refine::budget::RefineBudgetSampler`, state in
   `RefineLinkBudget`) with the QUIC path sample it already takes for video
   rate control, the interactive bytes it wrote (video; audio and input are
   small beside the headroom), the refinement bytes its refine writer handed
   to the lane (`arcen_transport::RefineLaneMeter`), its video backlog and
   mean frame wait. Refinement may use `min(max_mbps, session cap, ½ ×
   (delivered − interactive))`. *Delivered* is the larger of what the window
   carried and what the congestion window says the path delivers
   (`cwnd / RTT / 2`: a BBR cruise holds two bandwidth-delay products; for a
   loss-based controller this under-estimates, the safe side). The rate
   rises at most twofold a window (the first time from the video target);
   a back-off (below) halves the rate and
   caps it there, and that limit grows by half each clear window after.
2. **The video target is only the floor.** Until the first path sample the
   producer keeps the old cap (the encoder's target). The first
   `REFINE-RATE <bps> [HOLD]` (capenc control line, or the macOS worker's
   handle) replaces it, and `BITRATE` no longer moves refinement.
3. **Suspension (R6).** Refinement holds — bucket emptied and frozen,
   finished records cancelled and rescheduled, the refine writer waiting —
   for one quiet window after: a QUIC congestion event in a window with
   loss of at least 0.5 % (a random-loss floor fires an event per lost
   packet and is not congestion), loss above 1 % (two packets at least), a
   video backlog (a queued frame at the window's end or a mean frame wait of
   20 ms or more), or a frame wait rising by 5 ms or more above 10 ms.
   Congestion and loss also halve the limit, but only in a window in which
   refinement sent (a session start's keyframes in slow start are not
   refinement's doing). **Motion always wins:** the
   refine writer hands nothing to its lane while a video frame waits for
   the video writer or the budget holds it; the motion gate (§3) still
   cancels queued tiles as before.
4. **Queueing is probed before it is blamed.** Queueing above the baseline
   by more than 15 ms in two consecutive windows (or by 45 ms in one), the
   last of them one in which refinement sent at least 16 KiB, pauses
   refinement for one window — the evidence on which the video controller
   itself would cut. If the queue then falls to 5 ms or to
   half, it was refinement's: the limit halves. If it stays, it is the
   path's own (a wide-area path's RTT was measured drifting 30 ms with no
   load at all); refinement resumes at its rate and is probed again only
   by a further 15 ms rise.
5. **The video rate controller ignores self-caused delay.**
   `RateHints::refine_active` says refinement sent in the interval *and*
   pauses for the next one; a queue-delay or frame-wait cut in such an
   interval is deferred (`RateController::last_refine_deferral`), so the
   next interval, with refinement idle, decides. Refinement that goes on (a
   clear path, or queueing the probe found to be the path's own) never
   defers a cut. Loss is never deferred, at most two consecutive intervals
   are, and only an applied cut ends a run of deferrals (an event the cut
   cooldown suppresses does not), so real congestion is still answered
   within two seconds even if refinement did not pause.
6. **Telemetry.** Each Pier logs `video.refine.budget` (info when the
   source, the hold or the rate by a fifth changes; debug every window)
   with `rate_bps`, `source` (`floor` or `residual`), `hold` (`congestion`,
   `loss`, `video_backlog`, `frame_age`, `queue_delay_probe`, `none`),
   `delivered_bps`, `interactive_bps`, `holds`, `backoffs`,
   `video_deferrals`; the video controller's change line carries
   `refine_deferred`; capenc's and the macOS worker's refinement fields add
   `refine_rate_src` (`floor`/`link`) and `refine_holds`.
7. **Split Piers.** On Windows and macOS the budget runs where the
   encoder's rate controller runs (the agent), counting refinement where
   the agent hands it on and holding it there; the service's relay
   additionally keeps the refine writer behind any queued video frame. What
   the relay already holds (bounded by its backlog) may still drain during a
   hold.

## Consequences

- A paused Grading or HDR picture becomes the source's own code values on
  the Deck, proven by a digest rather than asserted, and the user can see it.
- Speed and every eight-bit zero-copy path are unchanged by construction:
  `resolve_refine_plan` refuses them, and nothing on that path reads the
  frame on the CPU.
- One more lane role, one more native dependency (behind a feature), three
  binary record types and two hello fields. The Deck spends up to 33 MB of
  GPU memory per 4K monitor while refinement is on.
- Bandwidth rises only while the picture is still and only up to half of
  what the link has to spare (refinement-budget amendment; before it, up to
  the session's own video bitrate). Measured on the first slice (one compression
  worker, LAN): a full-screen 1080p photograph is 2.5 MB and verified in
  about 2.4 s, a 4K one 10 MB in about 6.6 s (≈ 13 Mbit/s, bounded by the
  single worker rather than the 25 Mbit/s budget); desktop content with
  text and ramps at 1080p is 0.3 MB in 0.8 s. Motion frame rate and encode
  time are unchanged (30 fps, 14.6 ms per 4K frame, on and off).
- Refinement does not make motion better; it makes stillness exact.

## Alternatives rejected

- **Re-encode still regions at minimum QP / lossless HEVC.** NVENC's lossless
  mode is a whole-session preset, not per block, and still leaves the Deck
  unable to prove exactness; a minimum-QP refresh is near-lossless, not
  lossless.
- **Tiles in Y′CbCr for every Pier.** Converting an RGB source to Y′CbCr is
  itself rounding; the tile would be exact to a derived picture, not to the
  source.
- **Tiles on the video lane or the bulk lane.** Head-of-line blocking for
  motion, or for HID/USB/clipboard (§5).
- **PNG-style deflate or plain LZ4 tiles.** Measured: LZ4 needs about twice
  the bytes of `delta-zstd1`; deflate needs 56% more bytes at level 1 and is
  still 11% larger at level 6, at a fifteenth of the speed.
