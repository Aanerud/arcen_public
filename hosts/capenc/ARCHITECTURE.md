# hosts/capenc — `arcen-capenc`

**Delivery:** the cross-host capture+encode engine. Internal helper binary (not a marketed
product surface), spawned as a subprocess by **both** Arcen Piers.

## What this is

One fused native process captures the framebuffer and emits Annex-B video to
stdout. Fusion keeps capture, conversion, encoder lifetime, framing, and
recovery in one bounded process; it does **not** mean every quality mode shares
one pixel pipeline.

| Contract | Capture → conversion → encode | Copy boundary |
| --- | --- | --- |
| Linux Auto/Speed, 8-bit | NvFBC → CUDA → NVENC | Device-to-device; no host frame copy |
| Linux Grading, 10-bit SDR | Depth-30 Xorg → MIT-SHM packed RGB10 → shared RGB10-to-P16 conversion → CUDA → NVENC | Host capture/conversion plus one host-to-device upload |
| Linux HDR request on Xorg | Same as Linux Grading after the Pier changes transfer/primaries/matrix to BT.709 | Explicit degradation; capenc refuses PQ/HLG from Xorg |
| Windows Auto/Speed, 8-bit | DDA after real-image proof, otherwise WGC BGRA8 → D3D11 staging → negotiated YUV → NVENC | CPU-visible conversion path |
| Windows Grading, 10-bit SDR | WGC FP16 scRGB → shared SDR OETF/matrix → NVENC I444 P16 | CPU-visible FP16 conversion |
| Windows HDR | Verified HDR display state → WGC FP16 scRGB → shared linear-primary conversion + absolute PQ/BT.2020 → NVENC I444 P16 | CPU-visible FP16 conversion |
| Windows MF comparison path | Exact-output WGC BGRA8 → shared BGRA-to-NV12 → inbox Media Foundation H.264 | CPU-visible; not selected by the current packaged Pier policy |
| Portable OpenH264 | Windows WGC or Linux authenticated X11 BGRA → shared checked I420 → source-built OpenH264 | CPU capture/conversion/encode |

The paths stay separate because their source formats and truth differ. A new
wide path must not add a readback to Linux's eight-bit NvFBC fast path, and an
8-bit BGRA source must never be accepted as a ten-bit WGC/XShm source merely
because the destination buffer is wider.

## How it's grounded

  Provenance in `legal/ORIGINS.md`.
- Lessons encoded (critical):
  - **NvFBC captures the PHYSICAL GPU head, not the X server's logical framebuffer.** Two X
    servers on the same head capture identical pixels. Proven live: `:10` on DFP-0 grabbed
    `:0`'s pixels; moving it to DFP-1 + `--monitor 1` grabbed the real per-user desktop.
    Therefore each independently-captured screen needs its own head (DVI-D-0..3, ≤4).
  - **`--monitor N` → output index N-1.** `nvFBCGetStatus` enumerates the `$DISPLAY` server's
    RandR outputs; `TRACKING_OUTPUT` on `outputs[N-1].dwId`, else `TRACKING_SCREEN`. One head
    per Xorg ⇒ `--monitor 1` (index 0).
  - **YUV444 pitch/stride bug (known, real).** The YUV444 path sets `pitch = width` and
    `stage()` does one contiguous `cuMemcpyDtoD` of `w*h*3`, assuming NvFBC's
    `BUFFER_FORMAT_YUV444P` output is tightly packed. NvFBC returns rows at an **aligned**
    pitch; where pitch ≠ width, every row shears (1920 clean, 2560 sheared green). BGRA
    (pitch `w*4`) is unaffected. **Fix:** copy plane rows at NvFBC's real pitch
    (`FrameGrabInfo.dwByteSize` reveals padding), or register the NVENC input at that pitch.
    Add a regression test at a non-aligned width (2560/3600); the 4K path may hit it latently.
  - **Linux backend is feature-gated behind `--features nvenc`.** Building without it yields
    "no backend for this platform/feature combination" and the Pier disconnects at once.
    Always build the Linux engine with `--features nvenc`.
  - Buffer formats: NvFBC `BUFFER_FORMAT_YUV444P = 3` (planar `w*h*3`); NVENC
    `NV_ENC_BUFFER_FORMAT_YUV444`.
  - **Depth 30 does not imply one packed ordering.** The live GRID Xorg visual
    exposes red in bits 0–9 and blue in 20–29 (`XBGR2101010`). Derive shifts
    from the root visual masks; never cast depth-30 bytes to `BgraFrame` or
    assume `XRGB2101010`.

## Rules — what it must be (invariants)

1. Standalone binary. Emits Annex-B to stdout; framing/FEC stays in the Pier (Python-era
   guardrail: keep media relay-able — no client identity/routing baked into capenc output).
2. `unsafe` FFI is expected here (~1200 sites: NvFBC/CUDA/NVENC/DXGI/WGC/D3D11). This is why
   the workspace `unsafe_code` lint is `warn`, not `forbid`.
3. Linux eight-bit NvFBC keeps its device-to-device behavior. Linux depth-30,
   Windows BGRA/FP16 conversion, MF and OpenH264 are explicit bounded
   CPU-visible paths and must identify capture backend and copy cost in
   READY/telemetry.
4. Platform backends stay behind `cfg`; production Linux capenc uses
   `--features nvenc,software-h264`.
5. CLI stable: `--display :N`, `--monitor N`, `--fps`, `codec`, `yuv444`,
   `cursor=local|host`, framed, selftest.
6. Linux idle gating never removes the per-submission NvFBC→NVENC restage. Its one-deep
   NVENC pipeline receives a next-tick duplicate after first/activity/IDR so the newest
   frame cannot remain queued until keepalive.
7. `ERR_MUST_RECREATE` invalidates NvFBC's retained CUDA pointer and cadence state.
   The first stale NVENC output already in flight is dropped before the fresh IDR is exposed.
8. Linux emits versioned READY only after a real frame is captured, staged, and
   accepted by NVENC. READY includes the fixed cursor mode; the Pier must not
   advertise codec/chroma/backend/cursor before validating it.
9. Windows Host cursor mode is WGC-only. The WGC cursor toggle is strict; DDA is
   local-cursor-only and has no cursor-shape compositor.
10. Linux depth-30 XShm has no host-cursor compositor. The Linux Pier resolves
    Host to Local before native preflight and live spawn; eight-bit NvFBC keeps
    its existing in-video cursor path.
11. The default dependency graph contains no OpenH264/native build graph.
    `software-h264` enables only `arcen-media/software-h264-source`; no host
    module calls `openh264-sys2`.

## Interfaces / boundaries

- **Consumes:** `arcen-keel`, `arcen-media`, `arcen-protocol`,
  `arcen-telemetry`; GPU capture APIs (NvFBC/CUDA/NVENC on Linux;
  DXGI/WGC/NVENC/D3D11 on Windows).
- **Exposes:** an Annex-B stream on stdout, a `selftest` mode, and a finite
  `admission-v1` probe protocol that reports bounded per-frame queue age,
  encode latency, and delivery observations. Spawned by `arcen-pier-linux`
  (`media/capenc.rs`) and `arcen-pier-windows` (`capenc.rs`).
- **Control stdin** is line based and case insensitive: `idr` requests a
  keyframe, `wake` records one-shot input/focus activity for this region, and
  `stop` ends the run. Unknown lines are logged and ignored, so a Pier that
  never sends `wake` behaves exactly as before. A `wake` is advisory: it can
  only wake a region the activity scheduler had suppressed and never suppresses,
  delays, or downgrades a keyframe, a refresh deadline, or an admitted frame.

## Module map (from the proven source)

- `main.rs` — argv parse, backend selection, run loop, selftest.
- `linux.rs` — NvFBC capture including startup `bWithCursor`, idle submission
  cadence, INFO pipeline stats, and checked NvFBC 1.7 ToCuda ABI plus
  READY/UNAVAILABLE startup records.
- `nvenc_cuda.rs` — Linux CUDA↔NVENC staging (**the YUV444 pitch bug lives here**).
- `win.rs` — Windows capture selection. Eight-bit probes DDA before WGC;
  every ten-bit contract requires WGC FP16; HDR additionally verifies the
  bound output's PQ/BT.2020 DXGI state.
- `wgc.rs` — Windows.Graphics.Capture pool ownership. FP16 requests fail
  closed rather than becoming BGRA8 in a ten-bit container.
- `win_mf.rs` / `mf_encoder.rs` / `annexb.rs` — shared WGC staging plus
  Media Foundation or shared OpenH264 software encoding and framing. Portable
  BGRA conversion moved to `arcen-media`.
- `linux_x11.rs` — XRandR/XDamage/MIT-SHM 1.2 capture, depth-24 BGRA and
  depth-30 mask-derived RGB10 layouts, bounded GetImage degradation, checked
  modeset recreation, and shared OpenH264 loop.
- `region_schedule.rs` — host-neutral binding of `arcen-media`'s
  `RegionActivityScheduler` for both software capture loops. It owns the Keel
  damage tracker that already drives selective conversion, so one hash pass per
  frame yields the damage map, this region's serve/skip decision, and bounded
  fixed-field `activity_*` diagnostics on the per-second stats line. Suppression
  applies only to provably unchanged pixels: capture and hashing still run every
  tick, and startup baselines, client IDRs, recovery/modeset keyframes, the
  keyframe deadline, the max-idle refresh, and input/focus wakes are all
  mandatory services it can never skip. Encoder backend, codec/chroma, bitrate,
  and frame-rate policy stay host-authoritative.
- `nvenc.rs` + `nvenc_sys/{mod,guid,version}.rs` — Windows NVENC FFI and
  staging, including separate FP16-scRGB-to-SDR and FP16-scRGB-to-PQ row
  conversions.
- `frame_policy.rs` — keyframe/intra-refresh policy.
- `ARCEN_SESSION_LOG_ID` is an optional validated UUID inherited from the Pier.
  Diagnostics and per-second stats append `sid=<uuid>`; Annex-B stdout and the
  READY prefix remain byte-compatible.

## Deferred / roadmap

- **Fix the YUV444 pitch bug** with a non-aligned-width regression test (top capenc fix).
- Fine Linux damage needs a supported producer. Public NvFBC 1.7/1.9 ToCuda has
  no diff-map fields; use a separately reviewed ToSys/ToGL design or an original
  CUDA comparison kernel rather than guessing ToCuda ABI fields.
- Reference Frame Invalidation (loss recovery).
- Intel/AMD hardware Media Foundation encoder discovery, adapter binding, and async MFT loop.
- True multi-monitor Windows recreation/capture; the current host consumes the client's primary.
- OpenH264 physical fallback, 1080p30 performance/allocation, one-hour soak,
  and package distribution acceptance. No such results are claimed by the
  implementation.

## Resume pointer

- **Status:** ✅ MIGRATED + WINDOWS MF FALLBACK PROVEN. Built on pier-linux.example.internal with
  `--features nvenc`; NVENC selftest passed (3840×2160 IDR, CUDA ready). On
  `development workstation`, exact-output WGC + MF software H.264 streamed VMware SVGA at
  30 fps with working input; low-latency CBR, two-second GOP, zero B-frames,
  real force-IDR, and light agent logging passed user QA as smooth enough.
  Current MF negotiation maps a 1800×1168 client request to a 1792×1168
  macroblock-aligned Windows desktop rather than cropping encoded pixels.
  Linux Keel cadence and INFO stats are implemented offline; live GRID validation
  remains reviewer-owned.
- **Original next step (done):** copy `server/capenc/src` + `Cargo.toml` into `hosts/capenc`, rename crate/bin
  → `arcen-capenc`, keep the `nvenc` feature, wire into the workspace, and embed
  it in each Pier. Build the fused Pier on the target OS, run `capenc selftest`
  through that executable, then have the Pier spawn its own `capenc` subcommand.
