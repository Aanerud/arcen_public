# shared/keel - `arcen-keel`

**Delivery:** pure shared content-damage engine beneath every Arcen Pier.

## Phase-1 and Linux-parity contract

Keel accepts borrowed pixel frames and emits deterministic 16x16 block damage
metadata. It has no capture, conversion, encoder, transport, protocol, platform,
I/O, async, or serialization dependency.

The Windows Media Foundation adapter is the first consumer. It hashes a captured
BGRA frame, retains clean NV12 rows, and converts only dirty full-width block
rows. The encoder still receives a complete frame.

The Linux NvFBC/NVENC adapter consumes Keel's pure idle cadence. Driver-reported
new frames submit immediately, while a parked desktop submits a one-second
keepalive. `ExternalDamage` accepts future driver rectangles or one-byte block
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

## Modules

- `grid.rs`: checked frame/grid geometry and borrowed BGRA views.
- `activity.rs`: reusable damage wrapper, fixed rolling dirty ratio, content
  class, and semantic cadence recommendation.
- `cadence.rs`: deterministic first/activity/IDR/keepalive emission policy.
- `hash.rs`: enum-dispatched XXH3 and CRC32C kernels.
- `damage.rs`: reusable hash grid, dirty bitset, summaries, and iterators.
- `external.rs`: reusable conservative pixel-rect and source-block ingestion.
- `scenario.rs`: deterministic synthetic benchmark/test frames.

Encoder governors, refinement, tile ledger, and wire metadata remain later
phases and intentionally have no stubs here. Activity classification is a pure
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
