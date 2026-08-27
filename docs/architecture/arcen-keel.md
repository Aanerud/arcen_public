# Arcen Keel — the content-adaptive streaming engine (planning document)

**Status: PHASES 0–1 IMPLEMENTED; LINUX IDLE PARITY IMPLEMENTED OFFLINE —
PHASES 2–4 REMAIN ROADMAP.** `arcen-keel` (the keel is the structural backbone
under the pier) is the pure shared damage engine, with Windows software-H.264 selective
conversion and Linux NvFBC frame-level idle cadence. Written 2026-07-17 after
PRs #18/#19/#20; implementation status updated 2026-07-18.

## 1. Why this component should exist

Arcen now has two encode paths that both treat every frame as a full, opaque rectangle:

- **NVENC hosts** encode the full frame every capture tick. The GPU hides most of the
  cost, but bandwidth still spikes on keyframes (partially tamed by intra-refresh) and
  nothing distinguishes "4K parked on a file manager" from "4K full-screen playback".
- **Software H.264 hosts** CPU-convert and CPU-encode the full frame. PR #20 removed the
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
roadmap]: dirty-rects #1, RFI #2) plus the MF follow-ups parked in
`hosts/windows/todo_later.md`. Building it **once, as a pure shared crate**, is what
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
  `GetMovedRects` mapped onto the same grid — hardware-provided damage, no hashing.
- **NvFBC (Linux path):** frame-level `bIsNewFrame` drives idle cadence now.
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
on the two lab hosts during Phase 2, not adopted blindly.

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

The roadmap's endgame is the Rust QUIC ecosystem (quinn) via Arcen Span. Keel is
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
- The quinn spike itself lives in the dormant `gateway/` (Span) work and is *not*
  part of keel's milestones; keel refuses to create single-stream coupling.

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
  change. Recorded real-capture fixtures are a later *lab* task and never a
  prerequisite: the implementing machine has no access to the lab hosts. This corpus
  is the honesty check for every claim in this document.
- **SIMD policy:** safe Rust with `chunks_exact` first (the optimizer proved good
  enough in PR #20); hand-SIMD (`std::arch` behind runtime feature detection) only
  for the hash kernel and only if the corpus shows it matters. No nightly features.
- **Adapters stay thin:** if an adapter grows logic that a second host could want,
  it moves down into keel — that is the review rule that keeps hosts symmetric.

## 9. Phasing, prerequisites, and acceptance criteria

Already shipped (PR #20 baseline — do not re-plan): fused single-pass convert, clone
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
block rows; combined hash+conversion time ≥40% below the PR #20 full-conversion
baseline; steady full-damage bypass/probe overhead <5%.

Measured at 1792×1168 on the implementation VM: typing converted 2/73 block
rows (2.74%) and took 2.486 ms versus 4.922 ms full conversion (49.5% lower).
Sixteen bypass frames plus one probe cost 70.065 ms versus 67.845 ms for 17
baseline full conversions (3.3% overhead).

**Linux parity groundwork (implemented offline; lab review pending)**
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

Every acceptance criterion above that names a lab machine or an absolute CPU/bitrate
number is **lab-verified by the reviewer after merge** — §11 defines what the
implementing agent must prove *inside the PR* instead.

## 11. Execution brief — for an implementing agent with no lab access

The implementer works offline (no VPN: no pier-windows.example.internal, no development workstation, no
pier-linux.example.internal) and delivers **PRs against `main` for later review**. Everything in this
section is designed so a PR can be judged complete without touching a lab host.

### 11.1 Files — what to create, what to touch, what not to touch

New crate delivered in Phase 1 (`shared/keel` is a workspace member):

```
shared/keel/
  Cargo.toml          # name = "arcen-keel"; deps: xxhash-rust + crc32c.
                      # dev-deps: criterion.
  ARCHITECTURE.md     # distilled from this doc (§2–§7) when the crate is created
  src/lib.rs          # public API; #![forbid(unsafe_code)]
  src/cadence.rs      # first/activity/IDR/keepalive emission policy
  src/grid.rs         # BlockGrid: pixel dims → 16×16 block geometry, block-row spans
  src/hash.rs         # enum-dispatched XXH3 + CRC32C kernels
  src/damage.rs       # DamageTracker, DamageMap (bitset + iterators)
  src/external.rs     # pixel rect / one-byte driver-block ingestion
  src/scenario.rs     # deterministic synthetic scenario generators
  tests/golden.rs     # scripted scenarios → expected damage maps
  tests/properties.rs # seeded exhaustive checks (see 11.3); no new test-framework deps
  tests/allocations.rs# counting allocator proves a zero-allocation hot path
  benches/corpus.rs   # criterion: idle / typing / drag / scroll / video / full-damage
```

No empty `classify`, `governor`, `refine`, `ledger`, `meta`, or `config`
modules are created before the phase that consumes them.

Existing files touched per phase — and the guardrails:

- **Phase 0:** `hosts/capenc/src/mf_encoder.rs` (input IMFSample/IMFMediaBuffer reuse
  — verify with the SW MFT contract in a comment; fail back to per-frame allocation
  if the MFT holds buffers), `hosts/windows/src/display.rs` (one DXGI factory across
  the readiness polls), `hosts/capenc/src/win_mf.rs` (structured `PipelineStats`).
- **Phase 1:** `hosts/capenc/Cargo.toml` (dep on arcen-keel), `win_mf.rs` +
  `bgra_to_nv12.rs` (hash pre-pass, retained-NV12 dirty-block-row conversion,
  full-damage bypass and periodic refresh).
- **Phase 2:** `win_mf.rs` + `win.rs` encode loops (governor hookup);
  `hosts/windows/src/config.rs` + `capenc.rs` (the `keel` pier.json section,
  forwarded to the capenc subprocess as one compact-JSON CLI arg, `validate-config`
  coverage).
- **Phase 3:** `hosts/capenc/src/win.rs` (DDA dirty/move rects), `linux.rs`
  (supported external-damage producer), `nvenc.rs` (emphasis/QP-delta map, intra-refresh verify),
  `mf_encoder.rs` (ICodecAPI intra-refresh probe, fail-open); SVT-AV1 spike as a new
  optional cargo feature, isolated in its own module.
- **Phase 4 (only then):** `shared/protocol/src/messages.rs`/`wire.rs` (FrameMeta,
  TileRef, cache acks — additive, versioned), `clients/macos` (tile store +
  compositor), keel `ledger.rs`.

**Do-not-touch rules:** no protocol/wire changes before Phase 4; no Deck changes
before Phase 4; never edit the Linux host in a Windows-phase PR or vice versa;
keel itself never gains a platform dependency — if an adapter needs something
platform-shaped from keel, the *trait* moves into keel, the *implementation* stays
in the host.

### 11.2 Delivery contract — what a PR must contain

The first delivery intentionally combines Phases 0 and 1 in one reviewed PR;
later phases remain separate. Every PR must include, in its description:

1. **Local validation output** (pasted, since CI minutes may be dry): `cargo fmt
   --all --check`; `cargo test --locked -p arcen-keel` plus every touched crate's
   suite; `cargo clippy --locked -p arcen-keel -- -D warnings` (keel is a *pure*
   crate — it joins the strict-clippy gate from day one, unlike the FFI hosts).
2. **The corpus table:** criterion results for all scenarios, before/after when the
   PR changes a hot path (relative ratios, since absolute numbers are
   machine-specific). A hot-path PR without its corpus delta is incomplete.
3. **A "lab checklist" section:** the exact, copy-pasteable steps the reviewer runs
   on the lab hosts to verify the phase's lab-only criteria (which config to set,
   which log lines to expect, which stats fields prove the behavior).
4. **Scope confession:** anything from this document deliberately deferred, named.

Cross-compile sanity where the phase touches Windows hosts:
`cargo check --locked --target x86_64-pc-windows-msvc -p arcen-capenc --features nvenc,mf`
(works from macOS/Linux with the rustup target installed; no lab needed).

### 11.3 Testing — the four layers, all offline

1. **Property tests (keel):** seeded representative sweeps flip pixels at block
   corners/edges, tails, and stride boundaries; padding never participates in
   hashes; both kernels produce identical decisions. External pixel rectangles
   and driver-sized source blocks conservatively dirty every overlapping Keel
   block, including tails. This validates the implementation but does not claim
   a non-cryptographic hash cannot collide.
2. **Golden decision tests (keel):** scripted synthetic scenarios drive
   `DamageTracker`; readable hash and external-source damage maps and dirty-row
   sequences are asserted. Governor/refinery goldens land when those types exist.
3. **Corpus benches:** Keel measures both kernels, grid geometries, and external
   block-map ingestion; the MF adapter corpus measures the actual converter.
   Phase-1 acceptance is relative:
   typing must convert <10% of block rows and combined hash+conversion time must
   be ≥40% below full conversion; steady full-damage bypass/probe overhead must
   remain <5%.
4. **Adapter unit tests (hosts):** follow the existing `FakeBackend` pattern from
   `hosts/windows/src/display.rs` — pure capture/encode policy hooks assert the
   adapter honors decisions. Linux specifically proves no frame + no IDR + early
   keepalive means no submission, idle IDR is immediate, keepalive is one per
   second, and the one-deep NVENC pipeline flushes the final activity frame.

What is **explicitly not testable offline** and therefore reviewer-owned, per
phase: real WGC/DDA/NvFBC behavior, MF MFT quirks, absolute CPU/bitrate targets,
perceived latency/smoothness. The lab checklist (11.2.3) is the handoff for these —
the implementing agent writes the checklist, the reviewer executes it on
pier-windows.example.internal / development workstation / pier-linux.example.internal and closes the phase.

### 11.4 Linux parity lab checklist

1. Build and deploy the fused Linux Pier, whose `capenc` subcommand includes
   NVENC, and retain the pre-change Pier for an A/B.
2. Start a Deck session and confirm one WARN line reports
   `damage_source=unavailable_to_cuda`; no ToCuda diff-map setup is attempted.
3. Park the desktop for at least ten seconds. INFO `enc_fps=` lines should show
   `emit_keepalive≈1`, `encode_submitted≈1`, and bitrate far below the prior
   full-rate parked baseline.
4. Type, drag a window, and open menus. `emit_activity` should rise on the next
   pacing tick, followed by `pipeline_flush`; input latency and visual freshness
   must not regress.
5. Request a full frame while idle. `emit_idr` must rise immediately and the
   following pipeline flush must expose the IDR.
6. Run 4K60 full-motion playback. Compare fps, encode time, bitrate, and dropped
   frames with the retained baseline; cadence must remain activity-driven and
   must not alter NVENC settings.
7. Trigger an NvFBC modeset/recreate. Confirm `ERR_MUST_RECREATE` still rebuilds
   capture and streaming resumes, then disconnect and verify display/session
   cleanup.

## 10. Decisions

Resolved 2026-07-17 (user):

1. **Name/placement:** `arcen-keel` under `shared/keel`. Brand table gains:
   Keel = the content engine under every Pier.
2. **Hash function:** support both, pick by hardware probe at startup, default
   xxh3-64 (§3).
3. **Governor:** all constants tunable via a `keel` section in `pier.json`;
   defaults calibrated on the lab hosts (§4).
4. **Deck tile cache:** 100 MB default, configurable (§6).
5. **Phase-4 static-content strategy: option D committed** — RGB-exact lossless
   settle-patches over the H.264 base layer, shipped alongside the tile cache it
   shares plumbing with. The SVT-AV1 spike (option C) runs during Phase 3 to decide
   the base layer with data; option B stays a Phase-5 ambition gated on D's corpus
   results. The option analysis below is retained as the rationale.

**The Phase-4 static-content question, for the record:** after keel makes
H.264 as good as H.264 gets, what encodes *static* screen content on hosts where
pixel-exactness matters? (On NVENC hosts H.265 4:4:4 already keeps text sharp; this
decision mostly concerns software/MF hosts, which are stuck at H.264 4:2:0 — chroma
subsampling means colored text edges are never pixel-perfect, however high the
bitrate.) Options, effort-ordered:

- **A — H.264 only + keel refinement (the floor).** What Phases 1–3 deliver. Text
  gets "very good", never exact. No new client work. This ships regardless; the
  question is whether to stop here.
- **D — Lossless settle-patches (the recommended middle).** Keep H.264 as the base
  layer always. When a region goes static (damage map quiet), send its tiles once as
  losslessly compressed **RGB-domain** pixels (simple predictor + zstd/LZ4 — not a
  codec project, a compression call) and have the Deck composite them over the video
  until the region changes again. Sending BGRA source pixels bypasses YUV conversion
  entirely, so D answers *both* fidelity losses of the video path at once: 4:2:0
  chroma subsampling (blurred sub-pixel text — the measured pixel-level damage in
  published remote-desktop codec comparisons) and limited-range color-space
  conversion (gamut/range clipping that matters for color-critical VFX work). This
  is "build-to-lossless" with an exact final rung: parked UI text becomes
  pixel-perfect within a second or two. Needs the §6 compositor + one new payload
  type — it shares almost all of its plumbing with the tile cache, which is why
  it's the natural Phase-4 companion. Market corroboration: commercial tile
  codecs both converged on exactly this static/motion split from opposite
  starting points.
- **B — Full hybrid tile codec (best-in-class differentiator).** Keel segments the
  screen live: video-classified regions stream as H.264; UI/text regions skip H.264
  entirely and stream as our own progressive tile codec (lossy first pass →
  lossless build). Best-in-class sharpness *and* bandwidth, and the genuinely unique
  asset — but it is a real codec project: two-layer rate control, segmentation
  quality, months of corpus tuning. D is B's honest MVP; B is only worth starting if
  D proves the compositor + wire model and the corpus shows H.264 leaving real
  quality on the table.
- **C — AV1 base layer via SVT-AV1 (spike-worthy, orthogonal to D).** SVT-AV1 at its
  realtime presets (12–13) is a credible *replacement for the MF H.264 base layer*,
  with three strategic pros: **royalty-free** (H.264/HEVC licensing is a real cost
  for a commercial product; AV1 costs nothing), **cross-platform** (one software
  encoder for Windows *and* Linux — Linux currently has no software fallback at
  all), and **screen-content tools** (palette mode etc., built for our content). The
  protocol already reserved `supports_av1`. Cons that keep it honest: SVT-AV1 is
  **4:2:0 only**, and the chroma loss happens in our own BGRA→YUV conversion before
  any codec tool can help — so it does NOT deliver pixel-perfect text and does NOT
  replace D; realtime-preset CPU cost on thin-vCPU VMs must beat the MS MFT in the
  corpus before it earns its place; the Deck needs an AV1 decode path (VideoToolbox
  hw from Apple M3; dav1d software decode before that); and it is a C dependency
  behind a cargo feature, like nvenc. Hardware AV1 encode remains future-GPU
  territory (neither VMware SVGA nor GRID V100 has it).

**Decision (user, 2026-07-17): option D.** Run the **C spike (SVT-AV1 realtime
preset, screen-content mode, weakest-VM benchmark)** during Phase 3 so the Phase-4
gate can also decide the base layer; hold **B** as a Phase-5 ambition gated on D's
corpus results. The maximal coherent stack if everything measures well: SVT-AV1 base
layer + keel damage maps/governor + intra-refresh + D's lossless settle-patches.
Independent corroboration for D: published remote-desktop codec comparisons
measured text-pixel damage under 4:2:0, and commercial incumbents converged on
the static/motion split from opposite directions.
