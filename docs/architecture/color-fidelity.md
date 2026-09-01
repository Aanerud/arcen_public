# Colour Fidelity: 10-bit, 4:4:4 and Full Range

**Status: implemented and release-validated on `HDRReady` (2026-09-01).** The
shared vocabulary, wire, colour conversion, encoder signalling, four-preset
Deck surface, native capture providers, and ten-bit Deck presentation are
implemented.
The measured Windows wide path now captures DWM's FP16 scRGB composition
through WGC for every ten-bit stream. Grading converts that source to BT.709
10-bit SDR. HDR additionally provisions an HDR10 headless display before
capture, enables Advanced Color, proves the output is PQ/BT.2020, converts to
absolute BT.2020/PQ 10-bit 4:4:4, and presents the native VideoToolbox `xf44`
result through a dedicated `RGB10A2Unorm` HDR Metal path.
The measured Linux path now keeps ordinary eight-bit sessions on
NvFBC→CUDA→NVENC, while every depth above eight uses MIT-SHM against the
dedicated depth-30 Xorg screen and uploads explicitly converted P16 samples to
NVENC. That Linux path is genuine 10-bit SDR; HDR requests remain Windows-only
until the color-managed Wayland provider is implemented. Both current paths
have been exercised through the deployed Piers and the macOS Deck.
The manual release matrix covered all four presets on both hosts, nonzero
audio, input, cursor authority/degradation, restore, and credential-free
resume. Remaining platform expansion is listed under "Outstanding".
Findings from real hardware are recorded in
[`../testing/color-matrix-results.json`](../testing/color-matrix-results.json).
See [`../testing/README.md`](../testing/README.md) for how to run the probe
matrix end to end and record findings there.

## Canonical pipeline map

The presets are complete contracts. They do not toggle options inside one
capture implementation.

| Preset | Windows source pipeline | Linux Xorg source pipeline | Deck presentation |
| --- | --- | --- | --- |
| Auto | DDA after real-frame proof, otherwise WGC BGRA8 → negotiated 8-bit encode | NvFBC → CUDA → NVENC | SDR |
| Speed | Same 8-bit path at 60 fps | Same NvFBC device-to-device path at 60 fps | SDR |
| Grading | WGC FP16 scRGB → SDR transfer/matrix → HEVC I444 P16 | Depth-30 Xorg → XShm RGB10 → shared conversion → CUDA upload → HEVC I444 P16 | Native `xf44`, dedicated 10-bit Metal, EDR off |
| HDR | HDR EDID/topology and exact-target HDR proof → WGC FP16 scRGB → BT.2020/PQ → HEVC I444 P16 | No Xorg HDR provider: resolve to the Grading pipeline and report degradation | Native `xf44`, dedicated 10-bit Metal, PQ/EDR only when the resolved transfer remains PQ |

Software encoding is another separate pipeline. Windows MF consumes WGC BGRA8
through the shared NV12 conversion; source-built OpenH264 consumes checked
BGRA/I420. Neither software path silently claims Grading or HDR.

## Why this exists

Arcen's audience for this work is colour graders and VFX artists. Three
properties matter to them, in this order:

1. **4:4:4 chroma.** 4:2:0 discards three quarters of the chroma samples. On UI
   text, node graphs, scopes and thin mattes that produces coloured fringing and
   makes an eyedropper read the wrong value. This is a correctness problem, not
   an aesthetic one.
2. **10-bit depth.** For SDR desktops, two extra bits absorb RGB→YCbCr rounding
   error and make the round trip exact. For HDR, they are also required to carry
   useful PQ precision and highlights above SDR reference white.
3. **Full range.** Desktop content is natively full-range RGB (0–255). Limited
   range spans only 16–235, discarding roughly 14% of the code values before any
   coding loss, and cannot represent superblacks or superwhites distinctly.

## What was wrong before

- `capenc` wrote **no VUI at all**. Every stream it produced was untagged, so a
  decoder had to guess its range, and a decoder that guesses limited range on
  full-range content crushes blacks and clips whites.
- NVENC was fed `NV_ENC_BUFFER_FORMAT_ARGB` and performed the RGB→YCbCr
  conversion itself with an **undocumented, uncontrollable** matrix and range.
- `convert.rs` hardcoded BT.709 limited range directly into its integer
  expressions, so the encoder could not state what colour it produced because it
  could not produce anything else.
- `BitDepth` existed in the vocabulary but was hardcoded to `Eight` at every
  call site and **explicitly rejected** by the plan resolver.
- `ServerColorCaps.main10` existed on the wire and was hardcoded `false`.

## Capture precision depends on the platform and display state

Encoding an 8-bit source into 10 bits is still valuable, but Windows can now
provide a genuinely wider source when the requested display is in Advanced
Color mode.

- **Linux NvFBC** exposes six buffer formats in the public headers through API
  v1.9 — ARGB, RGB, NV12, YUV444P, RGBA and BGRA. All are 8-bit-class. There is
  no P010, Y410, RGB10A2 or FP16 to ask for.
- **Linux XShmGetImage** returns the dedicated Xorg screen's native pixels.
  With `DefaultDepth 30`, the measured NVIDIA root visual is 32 bpp with ten
  bits per component. Its masks are
  `R=0x000003ff/G=0x000ffc00/B=0x3ff00000`, so the live layout is
  `XBGR2101010` (red low, blue high), not the commonly assumed
  `XRGB2101010`. Arcen derives the channel shifts from the visual masks and
  refuses any ambiguous/non-TrueColor layout.
- **Windows Desktop Duplication** returns `B8G8R8A8_UNORM` whatever format list
  it is given. Asking for `R16G16B16A16_FLOAT` *exclusively* still returns
  BGRA8, and the call succeeds — so an implementation must branch on
  `GetDesc().ModeDesc.Format`, never on what it requested.
- **Windows Graphics Capture** delivers `R16G16B16A16Float` scRGB. Arcen
  requires that concrete pool format for every ten-bit Windows contract and
  fails closed if WGC refuses it; it never repacks a BGRA8 pool and calls the
  result ten-bit. For Grading, the linear SDR source is clamped to reference
  range and encoded with the requested SDR transfer. On the measured NVIDIA
  headless HDR path, the final HDR EDID makes HDR available, entering the
  distinct Windows HDR colour mode makes DWM compose in FP16 scRGB, and WGC
  captures that surface before downstream display-link quantisation.

On Windows 11, the legacy `advancedColorEnabled` flag is not an HDR verdict:
it can also describe WCG. Arcen uses
`DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO_2`, requests
`DISPLAYCONFIG_SET_HDR_STATE`, and starts capture only when
`activeColorMode == HDR`. Windows 10 retains the legacy API because it predates
the separate HDR/WCG state.

The NVIDIA headless EDID describes the virtual connector as HDMI-a/10 bpc and
includes HDMI deep-colour, HDMI Forum SCDC, BT.2020, HDR Static Metadata and an
explicit 1000/400/0.005-nit mastering envelope. This matters even though the
GRID virtual scan-out remains 8 bpc: before WGC starts, Arcen independently
requires `IDXGIOutput6` to report
`DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`. The link depth stays diagnostic;
the DWM surface captured upstream is FP16.

Headless EDID mutation remains owned by the display lease. Single-display
sessions keep the pre-provision recovery journal armed until teardown and
restore the original topology/EDIDs after the ordinary in-session display
transaction. HDR enablement is likewise scoped to the exact final
`(adapter,target)` identities resolved from that lease; an unrelated active HDR
monitor can neither satisfy the gate nor be toggled by the session.

## Windows FP16 scRGB to ten-bit SDR and HDR10

The WGC buffer is linear scRGB, not an already encoded BT.709 or PQ signal.
Grading and HDR deliberately use separate transforms.

For Grading, Arcen clamps linear components to `0.0..=1.0`, applies the
negotiated BT.709 or sRGB OETF, forms BT.709 YCbCr, and writes MSB-aligned
ten-bit samples. Values above reference white are clipped because the requested
stream is SDR; they are not mislabeled as HDR.

For HDR, the conversion order is load-bearing:

1. convert linear scRGB/BT.709 primaries to the negotiated linear primaries;
2. interpret scRGB `1.0` as Windows' 80-nit SDR reference white;
3. apply the SMPTE ST 2084 inverse EOTF, preserving scRGB values above `1.0`;
4. form the negotiated nonlinear YCbCr matrix and range; and
5. store each 10-bit code in the most-significant bits NVENC requires.

Applying an sRGB curve and merely labelling the stream PQ is not HDR: it maps
ordinary SDR white to PQ peak white and makes the desktop painfully bright.
Likewise, writing an unshifted code such as `0x0200` where NVENC expects
`0x8000` collapses neutral chroma towards zero and produces a green frame.
`ScrgbSdrTransform`, `ScrgbPqTransform`, and the wide-conversion tests pin these
invariants.

The conversion is a CPU fallback today, but not a per-pixel `powf` path. A
process-wide table stores the 1023 PQ half-code boundaries, each component uses
a binary search, and the frame is split across up to eight row workers. On the
Windows lab, 1800×1130 conversion fell from about 653 ms per fresh frame to
24.99 ms (40.02 fps) while remaining byte-identical between serial and parallel
conversion.

## Linux depth-30 capture and the HDR gate

Linux has two deliberately separate native pipelines:

| Requested depth | Capture and staging | Property preserved |
| --- | --- | --- |
| 8 bit | NvFBC → CUDA → NVENC | Existing device-to-device path; no host frame copy |
| 10 bit | XShmGetImage → host RGB10 conversion → CUDA → NVENC | Actual depth-30 source codes |

The split is keyed on bit depth, not transfer. Grading Reference is ten-bit
BT.709 SDR and needs XShm; otherwise it would receive an eight-bit NvFBC source
in a ten-bit encode. Conversely, an ordinary eight-bit session never pays the
XShm or CPU-conversion cost.

X11 defines precision and visual masks, but no desktop HDR composition space
or HDR metadata protocol. DaVinci Resolve's own Linux manual likewise limits
native HDR viewers to macOS and Windows. The Xorg provider therefore **does
not grant PQ or HLG**: an HDR request is resolved to the same HEVC 4:4:4
10-bit full-range BT.709 contract as Grading Reference, and the Deck reports
the changed matrix, primaries, and transfer as a permanent colour degradation.
It must not enter EDR for that session.

Real Linux desktop HDR is reserved for a future color-managed Wayland provider
that can prove an HDR composition space and capture ten-bit pixels with their
transfer/primaries metadata. Depth 30 alone is not that proof.

Measured on the Linux GRID V100D lab host:

- Xorg reported root depth 30, 32 bpp, little-endian pixels and MIT-SHM 1.2.
- A live frame sample contained 95,850 RGB components; 18,124 (18.9%) were
  outside the 256-value eight-bit expansion grid, proving source precision that
  an eight-bit capture cannot carry.
- At 2560×1600 with continuous motion, depth-30 Grading Reference capture
  sustained 30–31 fps through XShm, host RGB10 conversion/upload, and NVENC.
- The deployed Pier reports `capture=xshm capture_zero_copy=false`; Deck
  hardware-decodes the ten-bit 4:4:4 stream as `xf44` while remaining in SDR
  presentation mode.
- A separate deployed eight-bit smoke reported
  `capture=nvfbc capture_zero_copy=true` and decoded `444f`, proving the fast
  path remained independent.

## The 10-bit argument, measured

Encoding 8-bit RGB desktop content as **10-bit 4:4:4 full range** is
numerically lossless. This is asserted, not assumed:
`eight_bit_rgb_round_trips_exactly_through_ten_bit_444_full_range` in
`shared/media/src/video/convert.rs` sweeps the RGB cube and requires a
**per-channel maximum error of zero**. Its counterpart
`eight_bit_444_full_range_is_measurably_worse_than_ten_bit` requires the same
trip at 8-bit to be inexact, so the argument cannot quietly stop being true.

A second, broader assertion in `shared/media/src/test_pattern.rs` sweeps
**every `PROBE_MATRIX` row against every test pattern**. Measured
pure-transform error, per-channel, on the 0..=255 scale:

| Matrix / range / depth | Result |
| --- | --- |
| BT.709 limited 8-bit — *the format Arcen shipped before this work* | **never exact**; 1–2 codes on every pattern |
| BT.709 full 8-bit | exact on achromatic patterns, 1 code on chromatic |
| **BT.709 full 10-bit — the target** | **exact (0) on all five patterns** |
| BT.709 limited 10-bit | exact (0) |
| Identity/GBR full 10-bit | exact (0) |
| BT.709 full 12-bit | exact (0) |

That top row is the quantified case for the whole workstream: the format the
product shipped could not survive its own round trip.

This is why "Grading Reference" is 10-bit rather than lossless: it achieves an
exact round trip for the sources that actually exist at a fraction of the
bitrate.

**These are pure colour-conversion figures.** The end-to-end
`roundtrip_max_error` recorded by the probe harness additionally includes codec
quantisation loss and will not be zero for a lossy encode. The two numbers are
reported separately and must not be conflated.

## Negotiation model

Negotiate-best. The Deck states a preference; the host serves the richest plan
its backend can actually encode and reports precisely what it had to change
through `PlanDegradation`, which now carries `bit_depth_reduced`,
`range_changed`, `matrix_changed`, `primaries_changed`, and `transfer_changed`
alongside the existing fields.
`PlanDegradation::colour_degraded()` separates changes a colourist cares about
from an fps clamp they may not.

The production Deck exposes four complete presets rather than independent
performance and colour switches:

| Preset | Contract |
| --- | --- |
| Auto | 30 fps, adaptive 4:2:0 8-bit |
| Speed | 60 fps, adaptive 4:2:0 8-bit |
| Grading | 30 fps, HEVC 4:4:4 10-bit full-range BT.709 |
| HDR | 30 fps, HEVC 4:4:4 10-bit full-range BT.2020/PQ |

HDR is active only when the host returns PQ. A Linux Xorg Pier resolves the HDR
request to Grading and the Deck reports that permanent degradation while
remaining in SDR presentation mode. Legacy Full Colour 4:4:4 8-bit and
per-axis probe combinations remain developer-only controls.

Depth degrades to the **deepest** depth the backend can serve that is no deeper
than requested, so a 12-bit request on an NVENC host lands on 10, not on 8.

## Where colour lives

| Layer | Carries |
| --- | --- |
| `arcen_media::ColorRange`, `ColorMatrix`, `ColorPrimaries`, `TransferCharacteristics` | The vocabulary |
| `arcen_media::VideoConfiguration` | The selected contract (codec, chroma, depth, range, matrix, primaries, transfer) |
| `arcen_media::video::BackendLimits` | What a backend can encode, as sets |
| `arcen_media::video::ColorTransform` | The derived integer conversion |
| `arcen_media::video::VideoVariant` | One probe-matrix row, with a stable id |
| capenc READY v2 | `bit_depth`, `range`, `matrix`, `primaries`, `transfer` |
| `VideoHeader.flags` | Depth (bits 1–2), range (bit 3), matrix (bits 4–5) |
| `AuthResponse.initial_video` | Auth-time intent, requested axes, FPS ceiling, and measured Deck decode capabilities before host display/encoder creation |
| `server_hello.color_caps` | Host capability plus the resolved active format |
| `quality_settings` | Consistency echo of the authenticated video request plus audio quality controls; legacy clients still use it as their late request |

For current Decks, codec/colour selection is complete before the first encoder:
Auto and Speed authorize host-ranked AV1 → HEVC → H.264 for their fixed
eight-bit contract; Grading and HDR request HEVC 4:4:4 10-bit; and an explicit
developer variant remains an exact operator pin. Linux/L40S and
Windows/V100-to-M4 hardware runs prove the first `ServerHello` and the decoded
frame agree for both ordinary and grading intents.

### Why the frame header, and not just the handshake

Colour rides on **every frame** in the ten free bits of the existing `flags`
byte. The header does not grow. A decoder therefore never has to infer colour
from handshake state that may have been renegotiated by a resize or a respawn.
Zero flags decode as the 8-bit limited BT.709 contract every previous encoder
produced, and a reserved bit-depth encoding is **rejected** rather than read as
eight-bit, so a newer peer can never have its deeper format silently
misinterpreted.

`PROTOCOL_VERSION` moved to 4. There is no compatibility shim: a peer that
predates colour negotiation cannot state what colour it is producing, and
silently assuming 8-bit limited is exactly the failure this work removes.

## Two properties the conversion enforces

`ColorTransform` derives its coefficients rather than hardcoding them, and in
doing so guarantees two things the old constants only approximated:

- **Luma coefficients sum exactly to the luma scale**, so full white reaches the
  top code instead of landing one below it.
- **Each chroma triple sums exactly to zero**, so a neutral grey produces
  exactly the chroma centre. An eyedropper on a grey ramp is one of the first
  things a colourist does, and a one-code drift is visible there.

## MSB alignment

Both `NV_ENC_BUFFER_FORMAT_*_10BIT` and CoreVideo's ten-bit biplanar formats
store samples **MSB-aligned** in a 16-bit word: a 10-bit code `v` is stored as
`v << 6`, giving `0xFFC0` for white, not `0x03FF`. The Deck's live `xf44`
probe measured neutral chroma at `32768` (`512 << 6`), and the shift is pinned
by renderer tests.

## Hardware and OS limits

| Constraint | Consequence |
| --- | --- |
| `NV_ENC_BIT_DEPTH` defines only 8 and 10 | **No NVIDIA GPU encodes 12-bit.** 12-bit exists only through the software tier |
| `rav1e` measures ~3.1 fps at 1080p and ~0.67 fps at 4K, 4:4:4 10-bit, fastest preset | **The software tier is not interactive.** See below |
| NVENC exposes only `NV_ENC_AV1_PROFILE_MAIN_GUID` | AV1 4:4:4 is software-only |
| HEVC Main 4:4:4 10-bit works from Turing onward | The target format is available on every GPU generation Arcen targets |
| `NvEncReconfigureEncoder` cannot change depth or chroma | A format change requires a session recreate, which matches the existing capenc-respawn model |
| CoreVideo has no identity/GBR matrix constant | An identity stream cannot be described to any Apple API; it is a probe row, not a shipping path |
| Apple publishes no per-profile hardware-decode matrix | Whether a Mac decodes 10-bit Rext is **measured**, not assumed |

## The software tier is not interactive

`rav1e` is wired in behind the default-off `software-av1-source` feature. It is
the **only** route Arcen has to 12-bit or to AV1 4:4:4, because NVENC has
neither. That is the whole justification for it, and the measurements say
plainly that it is nothing more:

| Resolution | 4:4:4 10-bit, fastest preset (speed 10, low latency, 6 threads) |
| --- | --- |
| 1080p | ~3.1 fps |
| 4K | ~0.67 fps (>1.5 s per frame) |

That is one to two orders of magnitude below interactive. It must ship as an
explicit "maximum fidelity, expect multi-second latency" opt-in, never as a
default and never on a path advertised as smooth. Numbers were taken on a
shared development VM and are therefore a pessimistic bound, but the gap is far
too large for contention to explain.

`rav1e` needs **no** C toolchain or NASM: its `asm` and `git_version` default
features are deliberately not enabled, so the graph stays pure Rust and CI's
dependency-purity proof still passes with the feature off.

## Measured on target hardware

Windows NVENC host → M4 Pro Deck, 2026-08-15, commit `276e150`. Full evidence
in [`../testing/color-matrix-results.json`](../testing/color-matrix-results.json).
`err` is the end-to-end grey-ramp round trip, per channel, 0..=255.

| Variant | Encode | Decode | HW | Delivered | err |
| --- | --- | --- | --- | --- | --- |
| `hevc-444-8-limited-bt709` *(previously shipped)* | ok | ok | ✅ | `444v` | **2** |
| `hevc-444-8-full-bt709` | ok | ok | ✅ | `444f` | 0 |
| **`hevc-444-10-full-bt709`** *(the target)* | ok | ok | ✅ | **`xf44`** | **0** |
| `hevc-444-10-limited-bt709` | ok | ok | ✅ | `x444` | 0 |
| `hevc-420-10-full-bt709` | ok | ok | ✅ | `xf20` | 1 |
| `h264-420-8-full-bt709` | ok | ok | ✅ | `420f` | 1 |
| `h264-444-8-full-bt709` | ok | ok | ✅ | `444f` | 1 |
| `hevc-422-10-full-bt709` | unsupported | — | — | — | — |
| `hevc-444-10-full-identity` | ok | ok | ✅ | `xf44` | 1 |
| `av1-444-10-full-bt709` | unsupported | unsupported | — | — | — |
| `av1-444-12-full-bt709` | unsupported | unsupported | — | — | — |

### What this settles

**The target format works.** HEVC Main 4:4:4 10-bit full range encodes on
NVENC, **hardware**-decodes on an M4 Pro as `xf44`, preserves the full-range
VUI, and round-trips a grey ramp with **zero** error. The 10-bit Rext question
— the single highest-value unknown in this whole workstream, which Apple
documents nowhere — is answered: **yes, and in hardware.**

**The old format was measurably lossy.** `hevc-444-8-limited-bt709`, the format
the product shipped before this work, is the **only** row that loses two codes.
That is the empirical version of the argument this branch was built on, now
observed on real silicon rather than derived in a unit test.

**H.264 High 4:4:4 Predictive hardware-decodes on Apple silicon.** This was
expected to fail — Apple publishes nothing about it — and it did not. It is a
genuine finding, and it retroactively justifies opening `offered_chroma` to
H.264 4:4:4 rather than keeping the previous HEVC-only restriction.

**Identity/GBR survives CoreVideo.** The row decodes in hardware. Its `err=1`
is consistent with the knowingly-inaccurate BT.709 stamp Arcen is forced to
apply because `kCVImageBufferYCbCrMatrixKey` has no identity constant, so a
renderer that treats those frames as already-RGB should be able to reach 0.
Worth pursuing; not yet proven exact.

`hevc-422-10` is `unsupported` as predicted — 4:2:2 encode is Blackwell-only.
The AV1 rows are `unsupported` because the software tier was not built into
that run.



## A regression worth remembering

Target testing found Media Foundation's H.264 streams rejected by VideoToolbox
with `kVTParameterErr` (-6661), while FFmpeg's VideoToolbox path decoded the
identical captured bytes. The stream was fine; the Deck's handling was not.

Cause: the colour-extension override recreated the `CMFormatDescription`
through the **generic** `CMFormatDescriptionCreate`. A video format
description's dimensions are intrinsic state, *not* an entry in its extensions
dictionary, and that generic constructor has no width or height parameter at
all — so the recreated description was **0x0**, and VideoToolbox rejected any
session built from it.

Two things now prevent a recurrence:

1. The recreate uses the video-specific `CMVideoFormatDescriptionCreate`,
   carrying codec type and dimensions across explicitly.
2. A **geometry guard**: the recreate is discarded, with a logged reason, if
   the resulting dimensions differ from the source. A colour override is a
   colour operation and must never alter geometry, so that invariant is now
   checked rather than assumed.

Session creation additionally retries once without the destination
pixel-format request if VideoToolbox refuses it, so an unsupported format
degrades to VideoToolbox's native output — which the copy path already handles
— instead of losing the session. The retry is logged at warn, because it means
the negotiated format was not honoured exactly.

The wider lesson is that the failure was in the one step the implementing agent
explicitly flagged as unverifiable from a non-Apple machine. That flag was
correct, and it is why hardware testing is a gate rather than a formality.

The hardware retest at `1d202cc` is green: pier-windows-software.example.internal's MF H.264 stream
delivers `420v`, decodes in hardware, and displays in Deck without `-6661`.

That retest found a second regression before declaring success. Both product
Piers still constructed `VideoHeader.flags` from the keyframe bit alone,
despite the protocol reserving depth/range/matrix bits on every frame. A real
Linux Rext 10-bit full-range stream therefore reached Deck as the legacy
8-bit/limited contract and VideoToolbox delivered `444v`. Both hosts now call
`VideoHeader::encode_flags` from their resolved media plan for every frame.
The same live Linux stream then delivered `xf44-full`; host READY/hello truth is
not accepted as evidence without a decoded-frame check.

## Deck 10-bit HDR presentation

The Deck leaves egui on its ordinary 8-bit UI surface while rendering video
into a dedicated `CAMetalLayer`.
VideoToolbox's native biplanar `CVPixelBuffer` is retained instead of replacing
it with the CPU BGRA fallback. Metal wraps its luma and interleaved chroma
planes as `R16Unorm` and `RG16Unorm`, reconstructs the MSB-aligned ten-bit
codes, and performs the negotiated YCbCr-to-RGB matrix into an
`RGB10A2Unorm` drawable.

For PQ/BT.2020, the layer is tagged `kCGColorSpaceITUR_2100_PQ`, enables
extended dynamic range and carries HDR10 EDR metadata. Because
`RGB10A2Unorm` stores normalized PQ signal codes, `CAEDRMetadata` uses Apple's
required `10_000` optical-output scale: normalized code `1.0` is the ST 2084
10,000-nit reference peak. The VideoToolbox format description also receives
the negotiated PQ transfer constant rather than a matrix-derived BT.709
default, so any pixel-transfer fallback sees the same transfer truth. Ten-bit
BT.709 remains SDR; EDR is keyed on transfer characteristics, never depth.
Failure to create the native textures, pipeline, drawable or colour space is
reported as a typed fallback with a persistent warning for both Grading and
HDR, rather than silently claiming wide presentation on the 8-bit egui surface.

## The probe matrix

Uncertainty is resolved by measurement. Every open question is a row that gets
coded and run on real hardware, including rows expected to fail — a row that
fails is a recorded finding, whereas a row never attempted is an assumption.

Rows are defined in `shared/media/src/video/variant.rs` and selected with
`capenc variant=<id>`:

| Id | Answers |
| --- | --- |
| `hevc-444-8-limited-bt709` | Control: what Arcen ships today |
| `hevc-444-8-full-bt709` | Does the full-range flag survive the round trip? |
| `hevc-444-10-full-bt709` | **The target.** Does VideoToolbox decode 10-bit Rext, in hardware? |
| `hevc-444-10-limited-bt709` | Isolates range from depth |
| `hevc-420-10-full-bt709` | Main 10 fallback if Rext 10-bit does not decode |
| `h264-420-8-full-bt709` | Cheapest full-range win, widest client support |
| `h264-444-8-full-bt709` | Does VideoToolbox decode High 4:4:4 Predictive at all? |
| `hevc-422-10-full-bt709` | Blackwell-only encode; free to attempt elsewhere |
| `hevc-444-10-full-identity` | Can a GBR identity stream survive CoreVideo? |
| `av1-444-10-full-bt709` | Software tier |
| `av1-444-12-full-bt709` | The only 12-bit route that exists anywhere |

Note that `hevc-444-12-*` is deliberately **not** a row: it is rejected as
incoherent, because no encoder Arcen has can produce it.

## Outstanding

- **Linux desktop HDR needs the Wayland provider.** Xorg remains available for
  genuine 10-bit SDR grading, but PQ/HLG requests are downgraded truthfully.
  The future provider must prove compositor HDR state plus a ten-bit capture
  format carrying transfer/primaries metadata before Linux advertises HDR.
- **4:2:2 is not wired.** NV16/P210 bindings and BGRA conversion are absent,
  so the Blackwell capability question cannot yet be measured.
- The `rav1e` software tier has a real wrapper
  (`shared/media/src/video/software_av1.rs`, 4:4:4 at 8/10/12-bit,
  unit-tested, including the throughput benchmark this doc cites), and capenc
  exposes it only through the probe-only `software-av1` feature. No product
  Pier selects it as a runtime fallback.
- **Identity/GBR remains probe-only.** M4 Pro hardware-decodes the row, but
  CoreMedia exposes no matrix extension, so the client cannot describe it
  truthfully to downstream APIs.
