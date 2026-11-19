# shared/keel - `arcen-keel`

**Delivery:** pure shared content-damage engine beneath every Arcen Pier.

## Phase-1 and Linux-parity contract

Keel accepts borrowed pixel frames and emits deterministic 16x16 block damage
metadata. It has no capture, conversion, encoder, transport, protocol, platform,
I/O, async, or serialization dependency.

The Windows Media Foundation adapter is the first consumer. It hashes a captured
BGRA frame, retains clean NV12 rows, and converts only dirty full-width block
rows. The encoder still receives a complete frame.

The Linux NvFBC/NVENC and Windows DDA/WGC NVENC adapters share Keel's
`SubmissionGate`: a new frame or a requested keyframe submits immediately, one
duplicate follows to flush NVENC's output queue, and a parked desktop submits a
one-second keepalive. Measured on a Windows GRID host at 60 fps: an idle desktop
went from a submission every frame to one a second, about 15-25 kbit/s. `ExternalDamage` accepts future driver rectangles or one-byte block
maps without coupling Keel to a capture API.

## Invariants

1. `#![forbid(unsafe_code)]`; Rust 1.85; edition 2024.
2. Same frame sequence and kernel choice produce the same damage decisions.
3. No allocation after `DamageTracker` or `ExternalDamage` warm-up.
4. Row-pitch padding is never part of a block hash.
5. First frame is fully dirty.
6. Hash collision risk is probabilistic. Adapters must periodically perform a
   full refresh; Keel does not claim collision impossibility.
7. Keel owns no wire-visible type. Future metadata remains owned and versioned
   by `arcen-protocol`.
8. Activity history is a fixed eight-observation window; baseline refreshes do
   not enter that window, and updates allocate no storage after construction.
9. Narrowed hashing (`DamageTracker::update_within`) equals full hashing
   whenever the OS damage is a superset of the real change; an unreported
   change is found by the rotating audit stripe within `audit_period`
   narrowed updates. The first frame and every update after `reset` hash the
   whole frame.
10. Move vectors are hints. Their source and destination are always also in
    the damage bits; past `MAX_MOVE_RECTS` the ordered list is dropped whole.
    A consumer verifies a move exactly against what the receiver shows before
    it copies.
11. `BlockHistory` and `BlockTraitsMap` cost O(changed blocks) per update and
    allocate nothing after construction.
12. `MotionEstimator`, `BlockClassMap`, `MotionRegions`, `WorkMeter` and
    `TileLedger` allocate nothing after construction. Every move vector Keel
    reports, from pixels or the OS, is verified by the consumer
    (`verify_move`) before it becomes a copy.

## Modules

- `grid.rs`: checked frame/grid geometry and borrowed BGRA views.
- `activity.rs`: reusable damage wrapper, fixed rolling dirty ratio, content
  class, and semantic cadence recommendation.
- `cadence.rs`: deterministic first/activity/IDR/keepalive emission policy, and
  the `SubmissionGate` NVENC adapters submit through.
- `hash.rs`: enum-dispatched XXH3 and CRC32C kernels.
- `damage.rs`: reusable hash grid, dirty bitset, summaries, iterators, and
  narrowed hashing with the audit stripe.
- `external.rs`: reusable conservative pixel-rect and source-block ingestion,
  and the bounded, ordered move-vector list.
- `history.rs`: `BlockHistory`, one per-block clock: last change time and
  observation, a 16-observation change mask, and the last paint kind with its
  lossy flag.
- `traits.rs`: `BlockTraits` measured on changed blocks only: distinct colours
  up to 16, solid colour, and sharp-edge count (the text-like signal).
- `motion.rs`: `MotionEstimator`, vertical scroll and move search over
  retained row-segment hashes, and `verify_move`, the exact check.
- `classes.rs`: `BlockClassMap`, per-block `Static`/`Settled`/`Sparse`/
  `Motion`/`Moved` with hysteresis.
- `regions.rs`: `MotionRegions`, at most two 64-pixel aligned rectangles
  around `Motion` blocks, grow-fast/shrink-slow.
- `work.rs`: `WorkMeter`, EWMA milliseconds per megapixel per kind of work.
- `ledger.rs`: `TileLedger` and `TileKey`, the Pier's mirror of the receiver
  tile cache (XXH3-128 key → Pier-assigned slot, byte budget, LRU).
- `scenario.rs`: deterministic synthetic benchmark/test frames (including a
  text document scrolling in a viewport and an editor being typed into), the
  OS damage rectangles an honest compositor would report for them, and the
  true scroll offset.

Encoder governor policy and wire metadata remain with `arcen-media` and
`arcen-protocol`; Keel holds no stubs for them. Activity classification is a pure
scheduler input and does not alter encoder product behavior.

Classification is deterministic and conservative: zero current damage is idle;
an explicit source scroll hint is scroll; at least 75% current or 60% rolling
dirty blocks is full motion; and otherwise activity spanning at least 12.5% of
blocks and 50% of block rows is scroll-like. Remaining activity is sparse.

## Hash and adapter policy

WGC selective conversion requires a dedicated full-frame hash pre-pass; hashing
is not free inside color conversion because clean rows never enter the converter.
The XXH3 kernel hashes each pitch-separated active BGRA row with XXH3's optimized
short-input path, then position-mixes those row hashes into a 64-bit block
fingerprint. The explicit CRC32C kernel pairs its 32-bit CRC with an
independently seeded XXH3 high word; two differently seeded CRCs would be affine
for fixed-size blocks and would not provide 64-bit collision strength. `Auto`
resolves to XXH3 until measured CPU-class evidence justifies another choice.

The first Windows MF adapter uses 75%/25% converted-block-row hysteresis for
entering and leaving full-damage bypass. Conversion cost follows dirty rows, not
dirty-block count: one narrow vertical animation can still require full-frame
conversion. While bypassing it converts every new frame without hashing, then
probes every 16 frames. It fully hashes and converts on the first frame, forced
IDR, geometry reset, and at least every two seconds. These refreshes bound the
visible lifetime of a theoretical hash collision; they do not make collisions
impossible.

## 1792x1168 corpus evidence

Criterion release measurements on the initial 2-vCPU VMware implementation
machine:

| Path | Mean |
| --- | ---: |
| Full BGRA-to-NV12 conversion | 4.922 ms |
| Typing: XXH3 scan + selective conversion | 2.486 ms |
| 17 full-damage baseline frames | 67.845 ms |
| 16 bypass frames + one hash probe | 70.065 ms |

Typing changed 76/8,176 blocks and converted 2/73 block rows (2.74%); combined
hash plus conversion was 49.5% below full conversion. Full-damage probe overhead
was 3.3%. Absolute timings are machine-specific; the checked-in corpus exists to
compare ratios.

## Linux NvFBC policy

The Linux adapter keeps the proven NvFBC Shared CUDA -> NVENC path. NvFBC's
`bIsNewFrame` is sufficient for the immediate idle-cadence win; no pixel readback
or encoder configuration changes are required. Because Linux NVENC returns the
previous one-deep pipeline slot, first/activity/IDR submissions schedule one
next-tick duplicate so the final changed frame is not stranded until keepalive.

Public NvFBC 1.7 and 1.9 headers define `NVFBC_TOCUDA_SETUP_PARAMS` version 1 as
only `dwVersion` plus `eBufferFormat`; ToCuda grab params and frame info also
contain no diff-map pointer or geometry. Diff maps exist only on ToSys/ToGL.
Arcen therefore reports `damage_source=unavailable_to_cuda` and does not guess
fields or abandon zero-copy capture. A future Linux producer requires a reviewed
ToSys/ToGL architecture change or an original CUDA comparison kernel; either can
feed the existing allocation-free `ExternalDamage` API.

Criterion measurements for reset + one-byte block-map ingestion + summary on
the offline implementation machine:

| Frame / source map | Mean |
| --- | ---: |
| 1792x1168 idle | 30.929 us |
| 1792x1168 sparse | 26.675 us |
| 1792x1168 full | 157.91 us |
| 3840x2160 idle | 120.61 us |
| 3840x2160 sparse | 94.209 us |
| 3840x2160 full | 625.81 us |

These are machine-local review evidence, not cross-host performance promises.

## Canvas building blocks (K1)

The damage-first canvas design (`docs/architecture/arcen-keel.md` §11) needs
four facts from Keel beyond the damage map. Each is a pure function of
pixels, OS hints and time.

- **Move vectors.** `ExternalDamage::mark_move_rect` keeps the vector, clipped
  so source and destination both lie in the frame, besides marking both as
  damage. `PendingExternalDamage::observe_external` keeps them across
  coalesced captures, in order.
- **Narrowed hashing.** `DamageTracker::update_within(frame, os_damage)` and
  `ActivityGrid::update_within` hash only the reported blocks plus a rotating
  stripe of whole block rows (`DEFAULT_AUDIT_PERIOD` = 30 updates per sweep).
- **Block history.** `BlockHistory::observe(damage, now)` per captured frame;
  `record_paint` after the planner sends a block.
- **Block traits.** `BlockTraitsMap::update(frame, damage)` per captured frame.

Criterion on an Apple M4 Pro (release, XXH3, one synthetic frame
pair; `cargo bench -p arcen-keel --bench corpus -- narrowed_hashing`):

| Case | Full hash | Narrowed hash | History + traits |
| --- | ---: | ---: | ---: |
| 3840x2160 typing | 1.833 ms | 82.5 µs | 48.4 µs |
| 1920x1080 typing | 362.2 µs | 19.6 µs | 22.8 µs |
| 3840x2160 drag | 1.862 ms | 189.8 µs | 102.7 µs |

The existing full-hash corpus did not move (1792x1168 typing 387.0 µs before,
378.3 µs after). Machine-local review evidence, not a cross-host promise.

## Canvas building blocks (K2)

- **Motion search.** `DamageTracker::retain_row_hashes` keeps a
  position-independent hash of each 16-pixel row segment (256 bytes per
  block). `MotionEstimator::estimate` finds vertical offsets per band of
  changed block rows; `verify_move` is the exact check a consumer runs
  against the receiver's picture.
- **Classes.** `BlockClassMap::update(history, damage, verified_moves)`.
- **Regions.** `MotionRegions::update(classes)`.
- **Work meter.** `WorkMeter::record(kind, pixels, elapsed)`.
- **Tile ledger.** `TileLedger::insert`/`lookup`/`evict`, `TileKey::of`.

The XXH3 block hash now folds position-independent row-segment hashes (one
fixed seed for every row instead of a per-row seed) in order, so the segments
double as motion-search keys; the CRC32C kernel is unchanged and,
with row retention, hashes the rows once more.

Criterion, Apple M4 Pro, release, 3840x2160 (`cargo bench -p arcen-keel
--bench corpus -- canvas_analysis`):

| Case | Mean |
| --- | ---: |
| Full hash, whole-viewport scroll, plain | 1.35 ms |
| Same with row retention | 1.8 to 2.4 ms |
| Motion search on that scroll | 1.45 to 1.8 ms |
| Classes + regions, full-frame video | 0.21 ms |
| Tile ledger insert at a full 100 MB budget | 13 ns |
| 64x64 tile key | 0.60 µs |

The ranges are repeated runs on a shared machine. The 1792x1168 full-hash
corpus moved from 378 µs to 362 µs (typing) with the fixed-seed row fold.
