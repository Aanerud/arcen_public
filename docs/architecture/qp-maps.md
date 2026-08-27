# Damage-driven QP maps: Keel's 16×16 grid on hardware encoders

**Status.** Implemented and unit-tested; **never run on a GPU**. Off by
default. Selected per run with `qp-map=off|on|neutral`.

---

## 1. What was actually blocked

Nothing, as it turns out. NVENC has always exposed a per-frame QP map through
`NV_ENC_RC_PARAMS::qpMapMode` and `NV_ENC_PIC_PARAMS::qpDeltaMap`, and both are
present in the vendored **NVENCAPI 12.1** bindings. This was never a vendor
limitation or an SDK-version gate — Keel's damage map simply had never been
connected to them.

Before this change, Keel earned its keep in two places and neither was the
hardware encoder:

- it let the Windows **software H.264** path skip colour conversion for
  unchanged blocks, and
- it drove **idle cadence**, so a still desktop stops emitting frames.

NVENC received complete frames at a uniform QP, every time.

## 2. What this can and cannot buy

Worth being blunt, because the intuitive story is half wrong and the wrong
half is the expensive one to learn late.

**Not a gain.** It does not add NVENC session slots. It does not let us submit
only changed rectangles — NVENC still receives a whole frame. And it will not
deliver the naive "static desktop costs nothing" win, because *inter prediction
already does that*: an unchanged region is coded as skip with essentially no
residual. Raising QP on blocks that are already nearly free saves very little.

**Possibly a gain.** Spending the budget *down* on blocks that genuinely
changed — moving text, UI edges, a scrubbing timeline — so they get more bits
than a uniform-QP encode would give them. In a mostly-static VFX interface,
that is a small fraction of the frame, and the rate controller currently has no
idea which fraction.

That asymmetry is why [`QpBias::default`] is `dirty: -4, clean: +1` rather than
something symmetric. A large positive `clean` would buy almost no bitrate while
risking real damage: the occasional clean-looking block that *does* carry
residual gets coded badly and then persists as a reference for every frame
after it.

**Whether this beats uniform QP on real desktop content is unknown and
measurable.** It is not self-evidently a win.

## 3. Geometry

Keel produces a uniform 16×16 grid. NVENC wants one signed delta per *coding*
block, and that block is codec-specific:

| Codec | NVENC QP-map block | Keel blocks covered |
| --- | --- | --- |
| H.264 | 16×16 macroblock | 1×1 |
| HEVC | 32×32 CTB | 2×2 |
| AV1 | 64×64 superblock | 4×4 |

Entries are emitted in **raster order**, and dimensions **round up** — a
partial edge block still gets an entry, because NVENC sizes the map that way
and a short buffer would be read past.

A coding block counts as dirty when **any** Keel block it covers is dirty.
Deliberately conservative: under-marking starves a region that really changed,
while over-marking merely spends bits that were budgeted anyway.

Note the HEVC row. NVENC's QP map is addressed in 32×32 units regardless of the
CTB size the encoder actually chose, so this is NVENC's granularity rather than
the codec's. Because a mismatch here *silently misaligns every entry* instead
of failing, the map length is checked against the session's expected entry
count on every submission rather than trusted once.

## 4. Where the pieces live

```
arcen-keel                    arcen-media                     arcen-capenc
DamageTracker  ──────────►    QpDeltaMapBuilder    ──────►    qp_map.rs
(what changed)                (how a codec hears it)          (the seam)
                                                                   │
                                                                   ▼
                                                              nvenc.rs
                                                          qpDeltaMap per frame
```

`QpDeltaMapBuilder` takes a *dirty-block predicate*, not a Keel type, so
`arcen-media` needs no dependency on `arcen-keel` and the translator stays
exhaustively testable with no capture stack at all. The one invariant that
crossing costs — that both crates agree 16 is 16 — is asserted by
`keel_block_size_matches_the_translator_assumption`.

The **encoder owns the damage tracker**. `stage()` already holds the exact BGRA
frame about to be encoded, so observing damage anywhere else would risk
describing a different frame than the one the map is applied to. That is the
one mistake in this feature that produces a plausible-looking picture with the
bias on the wrong blocks, which is very hard to spot and very easy to blame on
the encoder.

## 5. Correctness decisions worth knowing

- **Suppressed on IDR.** Every block of a keyframe is coded intra, so
  "unchanged since the previous frame" describes nothing actionable — and a
  clean-region penalty applied there is baked into the reference that every
  following frame predicts from.
- **Suppressed without a fresh observation.** `restage_latest` and blank frames
  from the frame policy get a neutral map; a stale one describes a frame that
  has already been replaced.
- **Capability is probed, not assumed.** There is no `NV_ENC_CAPS_*` boolean
  for delta-map support (unlike the emphasis map, which has one and is
  H.264-only), so support is probed by **trial init**: request `qpMapMode`, and
  if `NvEncInitializeEncoder` refuses, log it and retry without. A GPU that
  says no still gets a working session.
- **Failures degrade, never break.** A tracker error or a build error costs
  that frame its bias, not the session its encode.
- **Bias is clamped** to ±10 QP steps. A delta map rides on top of whatever QP
  the rate controller chose, so an unbounded bias could drive a block to either
  extreme regardless of bitrate. This is a bias, not an override.

## 6. How to benchmark it

Three policies, because **the measurement needs a control arm**:

| `qp-map=` | Builds a map | Biases anything | Purpose |
| --- | --- | --- | --- |
| `off` | no | — | Shipped behaviour |
| `neutral` | yes, all zero | no | **Control**: isolates the cost of carrying a map |
| `on` | yes | yes | The real thing |

`neutral` matters. Comparing `on` against `off` alone conflates two different
things — the overhead of hashing every frame and submitting a map, versus the
effect of the bias itself. `neutral` versus `off` measures the first;
`on` versus `neutral` measures the second.

Measure, per the four axes that decide this:

1. **Bandwidth** — bitrate at matched quality. The headline number.
2. **Text sharpness** — the thing this is *for*. Bandwidth alone is not a
   result: a map that saves bitrate by blurring static text has failed. Judge
   on a text-heavy editor, a node graph, a scope — not synthetic patterns.
3. **Encode latency** — damage hashing is per-frame CPU work on the already
   CPU-mapped staging copy. It should be small; confirm it.
4. **Multi-monitor** — each monitor is its own capenc process with its own
   encoder and tracker, so cost scales with monitor count. Verify it scales
   the way you expect and that nothing diverges across the roster.

Suggested run, one variable at a time:

```sh
# control arm first, on the same content
capenc <args> qp-map=off
capenc <args> qp-map=neutral
capenc <args> qp-map=on
```

Watch the log line `QP map policy=<p> engaged=<bool>`. **`engaged=false` means
the feature is not running** — the driver refused `qpMapMode`, the codec has no
geometry, or the tracker could not be built. Do not record a result from a run
that never engaged.

## 7. What is not done

- **Benchmark conclusions are still open.** Neutral and On have both encoded
  live grading sessions with `engaged=true`, but matched workload numbers are
  still required before changing the default. Off now initializes with
  `NV_ENC_QP_MAP_DISABLED`, so Off/Neutral/On are distinct experimental arms.
- **Windows NVENC and the Linux CUDA ten-bit path.** Eight-bit formats on Linux
  CUDA cannot carry a map, and that is a decision rather than an omission.
  Damage hashing needs the frame on the CPU. On Windows every format already
  round-trips through a mapped staging texture, so tracking is free. On CUDA
  only `needs_own_conversion` formats — the two **ten-bit** ones — copy device
  to host for their own conversion; eight-bit formats stage zero-copy
  device-to-device. Engaging there would mean adding a full-frame readback
  purely to feed the map (tens of megabytes per frame at 4K) on the tier whose
  whole point is throughput, very likely swamping any bitrate saved and
  corrupting the benchmark it serves.

  **Practical consequence:** on Linux you can measure QP maps on the *grading*
  tier (HEVC 4:4:4 10-bit) but not on the *performance* tier (AV1/HEVC 4:2:0
  8-bit). Doing the latter needs either a CUDA damage kernel or an existing CPU
  copy to piggyback on — neither worth guessing at until the ten-bit numbers
  say whether the idea works at all.
- **No adaptive bias.** The bias is fixed per session. An obvious refinement is
  to scale it by the dirty ratio Keel already computes — spend harder when
  little changed, back off when everything did — but that is a second
  experiment and should not contaminate the first.
- **Sticky-QP risk unmeasured.** A region that goes clean and stays clean keeps
  whatever quality it was last coded at. With `clean: +1` this should be minor,
  and periodic IDRs wash it out, but it is the artifact to look for if
  something looks subtly wrong in a static corner.
- **AV1 superblock size assumed 64×64.** AV1 also permits 128×128. If AV1 maps
  misbehave while H.264/HEVC are fine, this is the first thing to check.
