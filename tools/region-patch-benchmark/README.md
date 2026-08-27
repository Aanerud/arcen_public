# Region patch research harness

`arcen-region-patch-benchmark` is a research-only workspace tool. It is not a
default member, does not change `arcen-protocol`, and does not select a live
carrier. The current full-picture region wire/default remains unchanged.

The harness treats `RegionActivityGrid::damage_map()` as the region-owned
activity map and derives all copies from its 16x16 dirty blocks. It reuses
Keel's seeded synthetic `idle`, `typing`, `drag`, `scroll`, `video`, and `burst`
scenarios. No recorded corpus, a local reference corpus content, compression library, or
platform API is used.

## Compared models

| Model | Complete carrier bytes | Source copies | Receiver/compositor |
| --- | --- | --- | --- |
| `full-picture` | Full active BGRA picture per emitted frame | Full picture | Not modeled |
| `dirty-rows` | Full picture; representative of current full-picture/ROI-QP framing | Coalesced full-width dirty 16-row bands into a retained picture | Not modeled |
| `dirty-rects` | Full picture; representative of a tighter retained-picture copy path | Horizontally coalesced runs, vertically merged when their x-span matches | Not modeled |
| `bounded-patches` | Only patch pixels plus conceptual metadata, or a full snapshot fallback | Coalesced dirty rectangles | Exact BGRA patches copied into a retained picture |

ROI/QP can change encoded allocation and quality, but it still consumes a
complete picture. This harness therefore gives the three complete-picture
models identical logical carrier bytes and measures only their different copy
work. It intentionally makes no compressed bitrate or quality claim.

Patch accounting uses a conceptual 32-byte frame header and 24-byte descriptor.
Those constants make metadata cost visible; they are not a proposed wire
layout.

## Bounds and reconstruction rules

- At most 64 non-overlapping patches are framed.
- A candidate above 64 patches or at least 80% of full-snapshot bytes becomes
  one full BGRA snapshot.
- Baseline, forced recovery, and the 120-tick (nominal two-second) recovery
  interval are full snapshots. A future generation, geometry, or color-contract
  change must do the same before any delta.
- Clean content is suppressed except for a 60-tick keepalive. Every model uses
  the same activity/cadence decision.
- Patches contain complete pixels for their rectangles and can be composed in
  either descriptor order.
- Frame sequence gaps mark the patch receiver unsynchronized. Later deltas are
  rejected until a full snapshot arrives.
- All frame, patch, rectangle, and payload storage is allocated at construction.
  Tests use a counting allocator and also reject any modeled capacity growth.

## Color constraints

The correctness oracle is byte-exact active BGRA reconstruction, including
tail blocks and alpha bytes. No matrix, primaries, transfer function, range,
chroma, or alpha conversion occurs here. Any future compressed patch design
must inherit or explicitly carry the keyframe's complete color contract; a
color-contract transition invalidates the retained base and requires a full
snapshot. A codec path that cannot prove pixel-equivalent color at patch
boundaries is a no-go.

## Deterministic byte/copy evidence

Command:

```text
cargo run --locked --release -p arcen-region-patch-benchmark
```

The fixed report is 640x360, 180 nominal 60 Hz ticks, seed 42. `copy` is
producer source-copy bytes. Patch compositor bytes equal patch source bytes.

| Scenario | Emits | Full carrier | Patch carrier | Patch saving | Full copy | Dirty-row copy | Dirty-rect / patch copy | Patch descriptors / full fallbacks |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Idle | 3 | 2,764,896 | 1,843,344 | 33.33% | 1,843,200 | 1,843,200 | 1,843,200 | 2 / 0 |
| Typing | 180 | 165,893,760 | 8,591,392 | 94.82% | 165,888,000 | 21,094,400 | 8,581,120 | 188 / 0 |
| Drag | 180 | 165,893,760 | 11,063,752 | 93.33% | 165,888,000 | 53,678,080 | 11,038,720 | 803 / 0 |
| Scroll | 180 | 165,893,760 | 165,898,080 | -0.003% | 165,888,000 | 165,888,000 | 165,888,000 | 180 / 179 |
| Video | 180 | 165,893,760 | 165,898,080 | -0.003% | 165,888,000 | 165,888,000 | 165,888,000 | 180 / 179 |
| Burst | 45 | 41,473,440 | 41,474,520 | -0.003% | 41,472,000 | 41,472,000 | 41,472,000 | 45 / 44 |

All 24 scenario/model runs had zero reconstruction mismatches, zero capacity
growths, and identical capture/emission/cadence counts within each scenario.
The allocation test measured zero allocations after construction and baseline
for every model.

Hash-only scroll damage is full-picture damage: without a moved-rectangle
producer, patches cannot recover scroll reuse. Burst's active frames are
full-motion snapshots, so it has the same limitation.

## Criterion evidence

Command:

```text
cargo bench --locked -p arcen-region-patch-benchmark --bench region_patch -- --noplot
```

Machine-local result on 2026-08-09: Apple M4 Pro, arm64, Rust 1.96.1. Each
measurement is the 1792x1168 transition after an untimed baseline; values below
are Criterion midpoint estimates. Absolute times are not cross-host promises.

| Scenario | Full picture | Dirty rows | Dirty rects | Bounded patches | Patch vs full |
| --- | ---: | ---: | ---: | ---: | ---: |
| Idle | 397.55 us | 396.90 us | 403.11 us | 420.29 us | +5.7% |
| Typing | 506.81 us | 402.85 us | 408.15 us | 426.56 us | -15.8% |
| Drag | 507.14 us | 428.24 us | 399.11 us | 424.74 us | -16.2% |
| Scroll | 502.67 us | 529.72 us | 527.77 us | 711.48 us | +41.5% |
| Video | 581.90 us | 535.50 us | 530.96 us | 626.40 us | +7.6% |
| Burst | 496.68 us | 503.16 us | 523.56 us | 665.09 us | +33.9% |

Full-damage patch frames copy the picture once into the patch payload and once
into the compositor, explaining their approximately 2x memory-copy volume.

## Go/no-go gates

These are research gates, not product SLAs:

| Gate | Threshold | Evidence | Result |
| --- | --- | --- | --- |
| Correctness | Zero mismatches for all scenarios, tail blocks, and reversed patch order | Zero; dedicated reconstruction tests | Go |
| Boundedness | <=64 patches, <=full snapshot plus metadata, zero post-warm-up allocations | Bound/fallback and allocator tests pass | Go |
| Cadence | Exact activity and emission parity with complete-picture models | Exact parity in all scenarios | Go |
| Sparse bytes | At least 50% below full picture for typing and drag | 94.82% / 93.33% lower | Go |
| Interactive bytes | Every typing/drag/scroll/burst case at least 10% below full picture | Scroll and burst have no saving | **No-go** |
| Sparse transition cost | No slower than full picture for typing and drag | 15.8% / 16.2% faster | Go |
| Full-damage transition cost | No more than 5% above full picture for idle, scroll, video, or burst | +5.7%, +41.5%, +7.6%, +33.9% | **No-go** |
| Color | Byte-exact active BGRA reconstruction | Exact in all correctness cases | Go for raw model |
| Recovery | Detect a lost delta, reject later deltas, recover on full snapshot | Dedicated loss/recovery test passes | Go |

**Decision: no-go for a live patch carrier.** Keep full-picture framing and
ROI/QP as the product default. Dirty-row/rectangle work remains useful as a
local retained-picture copy optimization. A future reconsideration needs a
reviewed moved-rectangle source for scroll plus real encoded
bitrate/quality/color and loss-latency A/B evidence; that external work is not
required to complete this offline harness.
