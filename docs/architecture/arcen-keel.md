# Arcen Keel — the content-adaptive streaming engine (planning document)

**Status: PHASES 0–1 IMPLEMENTED; LINUX IDLE PARITY IMPLEMENTED —
PHASES 2–4 REMAIN ROADMAP.** `arcen-keel` (the keel is the structural backbone
under the pier) is the pure shared damage engine, with Windows software-H.264 selective
conversion and Linux NvFBC frame-level idle cadence. Written 2026-07-17; implementation status updated 2026-07-18.

## 1. Why this component should exist

Arcen now has two encode paths that both treat every frame as a full, opaque rectangle:

- **NVENC hosts** encode the full frame every capture tick. The GPU hides most of the
  cost, but bandwidth still spikes on keyframes (partially tamed by intra-refresh) and
  nothing distinguishes "4K parked on a file manager" from "4K full-screen playback".
- **Software H.264 hosts** CPU-convert and CPU-encode the full frame. An earlier optimisation removed the
  gross waste (single-pass convert, no clones, idle keepalive), but a *single changed
  pixel* still costs a full-frame convert + a full-frame software H.264 encode.

Reference observations from a mature remote-desktop engine show what such systems do
differently, and none of it is encoder-specific — it is a layer of **content
intelligence between capture and encode**:

| Reference evidence (strings/config) | Technique |
|---|---|
| 8×8–128×128 block change-maps, `verify_changemap` | damage maps drive everything: only touched pixels cost anything |
| `"Frame encode took Xms so far. Dropping down the quality"`, `fps_estimator_algo` | measure encode time, degrade fps first, quality second — never stutter |
| `enable_build_to_lossless`, `build_to_lossless_target_sec`, quality floor 40 → lossless | changed regions start cheap and refine while static |
| `enable_tile_based_image_caching`, `temporal_cache_hits`, vertical-offset variant | the client never receives the same tile twice (even scrolled) |
| CPU-offload mode: AVX2 required both ends, auto-switch to GPU above ~10 MPPS | know the limits of software encode and gate on them |

This is the productization of two standing roadmap items ([reference-adaptation
roadmap]: dirty-rects #1, RFI #2) plus the Media Foundation follow-ups parked at
the time. Building it **once, as a pure shared crate**, is what
makes it pay on all hosts instead of becoming three divergent hacks.

**The product thesis:** for Arcen's market (VFX workstations — static, detail-dense
UIs with bursts of playback), perceived quality is dominated by (a) razor-sharp static
UI, (b) no stutter under load, (c) instant response to small changes. A damage-driven
engine optimizes exactly those three, and it is the part of the stack competitors
can't copy out of an ffmpeg flag.

## 2. What the component is (and is not)

`shared/keel` → crate **`arcen-keel`**. A **pure, platform-free Rust library**. No
`windows-rs`, no CUDA, no COM, no I/O. It consumes borrowed pixel data + timing
signals and produces *decisions and metadata*. All OS work stays in thin adapters
inside the existing host crates.

```
                       ┌────────────────────────────────────────────┐
 capture adapter ────► │                arcen-keel                  │ ───► encode adapter
 (WGC / DDA / NvFBC)   │                                            │      (MF MFT / NVENC)
  frames + dirty hints │  DamageTracker   → DamageMap (16×16 grid)  │       EncodeDirective
                       │  PacingGovernor  → fps/QP/refine schedule  │
 timing feedback ────► │  RefineryLadder  → build-to-lossless plan  │ ───► transport adapter
  (encode_ms, acks,    │  TileLedger      → client-cache bookkeeping│      (QUIC today,
   loss reports)       │  CapabilityGate  → sw-encode viability     │       richer QUIC later)
                       └────────────────────────────────────────────┘       FrameMeta
```

**Roadmap scope:** block hashing/damage tracking, frame classification (idle /
sparse-UI / scroll / video), the pacing governor, the quality/refinement ladder,
tile-cache ledger logic, and capability gating. **Implemented scope in Phases
0–1:** checked 16×16 damage tracking plus Windows software selective conversion,
external damage ingestion, and Linux frame-level idle cadence.
Future wire types such as `FrameMeta` and `TileRef` are defined and versioned by
`arcen-protocol`; Keel does not own or export wire contracts.

**Out of scope (stays in adapters):** capturing, color conversion, calling encoders,
sockets. Keel *tells* the MF adapter "convert block-rows 12–14 only, encode this tick
at fps 24"; it *tells* the NVENC adapter "emphasis-map these MBs, invalidate ref N";
it never touches an API itself.

**Invariants (design-first, per repo rule):**
1. `#![forbid(unsafe_code)]`, zero platform deps — tests and benches run on the
   macOS dev box and in every CI lane.
2. Deterministic: same frame sequence + same feedback ⇒ same decisions (golden tests).
3. Zero allocation on the per-frame path after warm-up (grids and scratch reused).
4. Wire-visible types are versioned in `arcen-protocol`, not in keel.
5. Every optimization is **measured before merge** against the recorded-scenario
   corpus (§8); a keel change without a benchmark delta is not landable.

## 3. The damage map — the piece everything else stands on

One abstraction, three producers, four consumers.

**Model:** a 16×16-pixel block grid (1800×1168 → 113×73 ≈ 8.2k blocks; the grid is
~66 KB of u64 hashes — trivial). Two layers:
- `DamageMap` for the current tick: bitset of blocks whose content changed.
- `BlockHashGrid`: last-seen 64-bit hash per block to detect change. Both an
  XXH3 row-composite kernel and a CRC32C-plus-independent-XXH3-high-word kernel
  are compiled in. Two differently seeded CRC32Cs are affine for a fixed block
  length and would provide only 32-bit collision strength. `Auto` currently
  resolves to XXH3; CRC32C remains explicitly benchmarkable and is not selected
  automatically until a measured CPU-class allow-list exists. The choice is
  process-local and never wire-visible.

**Producers (adapters):**
- **MF/WGC (Windows software path):** WGC exposes no cross-version dirty-rect
  contract, so selective conversion pays a dedicated full-frame hash pre-pass.
  Hashing is not “free inside conversion”: clean rows deliberately never enter
  the converter. The adapter retains complete NV12 planes and converts only
  dirty full-width 16-row bands. At high damage it bypasses hashing, with one
  probe every 16 frames; first frame, forced IDR, geometry reset, and a
  two-second periodic backstop always hash and fully convert.
- **DDA (Windows NVENC path):** `IDXGIOutputDuplication::GetFrameDirtyRects` +
  `GetFrameMoveRects` mapped onto the same grid — hardware-provided damage, no
  hashing or readback. Move rects dirty the destination and, conservatively, the
  source reveal.
- **WGC (Windows NVENC fallback):** on Windows 11 24H2+ the adapter sets
  `GraphicsCaptureSession.DirtyRegionMode = ReportOnly` and folds every
  `Direct3D11CaptureFrame.DirtyRegions` list into the same `ExternalDamage`
  accumulator. Latest-wins queues merge superseded raw-frame regions into the
  newest encoded frame. Older runtimes have no WGC damage source, so the QP map
  path reports `damage_source=none` and does not submit a neutral map as if it
  were engaged.
- **NvFBC (Linux path):** frame-level `bIsNewFrame` drives idle cadence now,
  and `ArrivalPacer` submits each new frame as it arrives, paced to the
  contract (a fixed contract-rate tick lost a third of a 60 Hz head's frames;
  ADR 0014 amendment 12).
  Public NvFBC 1.7 and 1.9 expose diff maps only through ToSys/ToGL; the existing
  zero-copy ToCuda setup has no `bWithDiffMap`, map pointer, or geometry fields.
  Fine damage therefore needs a separately reviewed ToSys/ToGL design or an
  original CUDA comparison kernel. Either can feed Keel's external block-map API.

**Consumers:**
1. **Convert scheduler (MF):** skip clean block-rows in BGRA→NV12 (biggest remaining
   software-path CPU win — cost becomes proportional to changed pixels).
2. **Pacing governor:** changed-MPPS is the load signal (§4).
3. **NVENC hints:** per-MB emphasis/QP-delta maps (NVENC exposes these) so static
   regions spend fewer bits and changed regions get them; pairs with the already
   shipped intra-refresh. Reference Frame Invalidation (roadmap #2) uses the same
   feedback plumbing when loss reports arrive.
4. **Tile ledger (§6):** block hashes are exactly the cache keys.

**Honesty note:** with a stock MF H.264 MFT we cannot encode partial frames — the
encoder always sees the full picture. The MF wins come from (a) skipping conversion
of clean blocks, (b) frame-level skip/cadence (shipped), (c) governor decisions. The
*encoder-level* payoff of damage maps lands on NVENC (emphasis maps, RFI) — the same
map, more leverage. If we ever want true region encoding on CPU, that is the Phase-4
hybrid-codec decision (§7), not a patch on the MFT.

**Collision note:** the 64-bit hashes are probabilistic. Tests prove that the
implementation detects representative pixel changes and excludes pitch padding;
they cannot prove mathematical collision impossibility. Production does not
memcmp every equal block because that would erase the static-content benefit.
The mandatory two-second full conversion bounds any stale retained NV12 row.

## 4. The pacing governor — degrade fps before quality, never stutter

Inputs per tick: EWMA of convert_ms + encode_ms (MF) or encode_ms (NVENC), frame
budget (1000/fps), changed-MPPS from the damage map, transport backpressure (queue
depth today; ack/loss signals under QUIC).

Policy adapted for Arcen:
- **Load ladder:** when p95 pipeline time exceeds ~70% of budget, step fps down
  (30→24→20→15 floor on MF; 60→48→30 on NVENC) *before* touching quality; recover
  hysteretically. This formalizes what the idle keepalive already does for the
  zero-change case, generalized to "some change, limited CPU".
- **Quality ladder:** only under sustained overload at the fps floor, step bitrate/QP
  down one notch at a time; restore quality first, fps second, on recovery.
- **Content classes:** `Idle`, `SparseUI` (few dirty blocks — favor quality + high
  responsiveness), `Scroll` (moved-rects/offset-hash detection), `Video` (sustained
  large damage — favor fps, accept lower QP). Class switches are rate-limited.
- **Viability gate:** sustained changed-MPPS above a calibrated ceiling (~10 MPPS in comparable CPU-offload designs) on the software path ⇒ clamp class to `Video` behavior
  and surface a `capability_degraded` note in telemetry/ServerHello-style reporting
  rather than melting the vCPUs.

Telemetry already exists (per-second `enc_fps/avg_encode_ms/max_encode_ms` lines);
the governor formalizes it into `PacingDecision` and the stats line grows the current
class + ladder position so live QA can see it.

**Decided:** every governor constant (fps floors and steps, ladder thresholds,
refinement window, viability ceiling, hysteresis) ships as a tunable `keel` section
in `pier.json` with calibrated defaults — same pattern as the existing
`desktop`/`video` sections, validated by `validate-config`. Defaults get calibrated
on real Linux and Windows hosts during Phase 2, not adopted blindly.

## 5. Progressive refinement — build-to-lossless, approximated in H.264

Comparable remote-desktop engines re-encode settled regions at increasing quality until perceptually lossless.
Stock-H.264 approximation, in order of payoff:
1. **Frame-level QP ramp (cheap, both encoders):** when the damage map goes quiet,
   schedule 2–4 "refinement ticks" at stepped-up quality (MF: temporarily raised
   bitrate/lowered QP; NVENC: qpDelta relaxation on recently-changed MBs), then
   return to idle keepalive. A parked desktop converges to a crisp image instead of
   freezing on the last motion frame's quality.
2. **NVENC emphasis inversion:** during refinement ticks, invert the emphasis map —
   spend bits on the *recently changed, now static* blocks.
3. **Intra-refresh everywhere it exists:** replaces the periodic-IDR bandwidth spike
   with a rolling intra column — proven on NVENC in prior measurements (3.2× static-detail
   peak reduction, −29% avg bitrate). Verify the arcen-capenc lift carries it on the
   NVENC path; probe the MF MFT's `ICodecAPI` intra-refresh knobs at runtime
   (`IsSupported`, fail-open to GOP keyframes). Bandwidth smoothing + loss-recovery
   only — it does not change pixel fidelity and does not compete with §10's
   static-content options.
4. True per-tile lossless build is the Phase-4 hybrid question (§7).

## 6. Tile ledger + client cache — the Deck side (bigger, staged last among the core)

Keel keeps a `TileLedger`: block hash → cache-slot state, mirrored with the client
via acks. When a "new" block's hash matches a slot the client already holds, the wire
carries a `TileRef` (slot id + position) instead of pixels; a vertical-offset probe
catches scrolling (tracked as zero-offset/offset-tile cache hits). Requires:
- `arcen-protocol`: `FrameMeta`/`TileRef` messages + cache-ack channel (versioned,
  additive; harmless to the current QUIC stream).
- Deck: a bounded tile store (**decided: 100 MB default**, configurable in Deck
  settings) + compositor step — this is the largest client change and the reason the
  ledger ships after §3–§5.
- Eviction is ledger-driven (LRU + explicit invalidate), and the ack channel is the
  same feedback path RFI and QUIC loss reports use — one plumbing job, three tenants.

## 7. Transport alignment — current QUIC, richer QUIC later

The transport is the Rust QUIC ecosystem (quinn). Keel is
where the *content* meets the *transport*, so its output types are designed now to
map cleanly later — with zero behavior change on today's QUIC single stream:

- `FrameMeta { class, damage_ratio, refinement_stage, deadline_hint, tile_refs }`
  rides beside each AU. On the current QUIC profile it is telemetry + tile
  refs. In a richer QUIC profile it becomes routing
  policy: control + tile acks on the reliable stream; AUs on per-frame uni-streams
  (droppable by deadline); small delta/refinement payloads eligible for RFC 9221
  DATAGRAMs; loss feedback → RFI + ledger invalidation.
- Rule now: **nothing in keel or the adapters may assume ordering or reliability
  beyond "AUs arrive or are reported lost"** — that keeps future QUIC profile
  changes a transport-adapter swap, not an engine rewrite.
- Keel refuses to create single-stream coupling.

## 8. Maintainability — how this stays healthy across three hosts

- **Placement:** `shared/keel` next to `shared/protocol`; same design-first
  discipline (its `ARCHITECTURE.md` is this document, refined at build time).
  Dependency rule: hosts → keel; keel → (nothing but std + a hash crate);
  future wire types remain owned by protocol; neither shared crate depends on the
  other in Phases 0–1.
- **Testing:** golden decision tests (synthetic frame scripts → expected
  damage sequences); property tests exercise representative pixel changes at
  block/tail/stride boundaries and compare both kernels' decisions. These tests
  establish implementation sensitivity, not collision impossibility. Governor,
  refinement, and ledger tests land with those phases.
- **Benchmark corpus:** deterministic **synthetic** scenario generators (idle
  desktop, terminal typing, window drag, scroll, full-screen video — procedurally
  drawn, seeded, no binary fixtures in the repo) replayed through criterion on every
  change. Recorded real-capture fixtures are a later task on real hosts and never a
  prerequisite. This corpus
  is the honesty check for every claim in this document.
- **SIMD policy:** safe Rust with `chunks_exact` first (the optimizer proved good
  enough in the earlier convert optimisation); hand-SIMD (`std::arch` behind runtime feature detection) only
  for the hash kernel and only if the corpus shows it matters. No nightly features.
- **Adapters stay thin:** if an adapter grows logic that a second host could want,
  it moves down into keel — that is the review rule that keeps hosts symmetric.

## 9. Phasing, prerequisites, and acceptance criteria

Already shipped (baseline — do not re-plan): fused single-pass convert, clone
removal, idle keepalive, MFT buffer-size cache, drain-scratch reuse.

**Phase 0 — debts + instrumentation (implemented)**
Input `IMFSample`/`IMFMediaBuffer` reuse investigation (MFT may hold buffers —
verify with the SW MFT before trusting it); DXGI-factory reuse across the
session-start readiness polls; promote the per-second stats into a structured
`PipelineStats` that Phase-2 consumes. *Accept:* no per-frame COM allocation on the
happy path; session start does ≤3 factory creations; stats visible in agent log.

The inbox synchronous H.264 MFT reports that it does not hold input buffers.
Arcen now reuses one input and one caller-owned output sample/buffer pair after
warm-up and reports allocations/reuses. The display lease caches one
`IDXGIFactory1`, recreating it only when stale or after topology mutation.

**Phase 1 — keel crate + damage maps on the MF path (implemented)**
Create `arcen-keel` (grid, hashes, DamageTracker, golden tests, corpus harness).
MF adapter: hash-compare pass + dirty-block-row-only conversion.
*Accept (corpus, 2-vCPU class VM):* terminal-typing scenario converts <10% of
block rows; combined hash+conversion time ≥40% below the full-conversion
baseline; steady full-damage bypass/probe overhead <5%.

Measured at 1792×1168 on a test VM: typing converted 2/73 block
rows (2.74%) and took 2.486 ms versus 4.922 ms full conversion (49.5% lower).
Sixteen bypass frames plus one probe cost 70.065 ms versus 67.845 ms for 17
baseline full conversions (3.3% overhead).

**Linux parity groundwork (implemented)**
NvFBC `bIsNewFrame` now drives the same first/activity/IDR/one-second keepalive
cadence, without changing CUDA restaging or NVENC configuration. Structured
pipeline stats are INFO-visible. Keel accepts conservative external pixel rects
and one-byte source-block maps with zero allocation after warm-up.

**Phase 2 — pacing governor (both Windows encoders)**
Ladders, classes, viability gate, hysteresis; wired into MF loop and the NVENC
run_encode loop.
*Accept:* under synthetic CPU starvation the MF path shows no frame gap >150 ms and
walks 30→15 fps gracefully; class + ladder position visible in stats; NVENC path
unchanged when unloaded.

**Phase 3 — NVENC damage integration (Windows DDA + Linux producer)**
DDA dirty/move rects and a supported Linux damage producer both feed the same
grid; emphasis/QP-delta maps on changed vs static MBs; refinement ticks (§5);
intra-refresh verified/ported on NVENC and probed on MF (§5.3); groundwork for RFI
(feedback plumbing; actual RFI lands with loss reports).
*Accept:* static-detail corpus bitrate ≥30% below today at equal PSNR on changed
regions (methodology per the intra-refresh A/B: mandelbrot still + moving overlay);
Linux and Windows produce equivalent DamageMaps for the same scripted scene.

**Phase 4 — decision gate, then the wire: tile cache + QUIC-shaped metadata**
Ship `FrameMeta`/`TileRef` in protocol; Deck tile store + compositor; scroll-offset
probe. Separately, the **hybrid-codec decision**: whether Arcen builds its own
lossless-text/tile codec for static content composited with H.264 for motion (the
true differentiator) — decide *after* Phases 1–3 ship, with corpus data on how
far H.264 + refinement actually gets us.
*Accept (cache):* scripted scroll + menu scenario ≥30% wire-bytes reduction; cache
disable flag ships (fail-open to plain AUs).

Ordering rationale: 1→2 are host-side only and immediately visible on the VMware
box; 3 reuses 1's abstraction where the hardware already helps; 4 is the only phase
that touches protocol + Deck and therefore rides behind everything provable
server-side. Multi-monitor M2 is orthogonal (per-monitor pipelines each own a keel
instance) and neither blocks nor is blocked by this plan.

Acceptance criteria that name absolute CPU or bitrate numbers are verified on
real hosts; unit and offline tests cover the rest.

## 10. Design choices

1. **Name and placement:** `arcen-keel` under `shared/keel`; Keel is the
   content engine under every Pier.
2. **Hash function:** both supported, picked by a hardware probe at startup,
   default xxh3-64 (§3).
3. **Governor:** all constants tunable via a `keel` section in `pier.json`,
   with measured defaults (§4).
4. **Deck tile cache:** 100 MB default, configurable (§6).
5. **Static content:** RGB-exact lossless settle-patches over the H.264 base
   layer, sharing plumbing with the tile cache; an SVT-AV1 base layer is
   evaluated with data before any change of base codec.
