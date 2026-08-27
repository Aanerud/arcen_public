# Colour Fidelity: 10-bit, 4:4:4 and Full Range

**Status: in progress on `feat/color-fidelity-10bit-444`.** The shared
vocabulary, wire, conversion, NVENC (including its BGRA→YUV444/NV12/P010
conversion, replacing the old ARGB feed), MF and OpenH264 colour signalling,
the host colour-policy config surface, and the Deck settings surface (presets,
Advanced overrides, and a probe-matrix variant picker) have landed. The Deck
WGSL render path, both matrix tools, and host-side colour config wiring have
also landed and were compiled on all three target platforms. The final drawable
is still 8-bit and several live-path gaps remain — see "Outstanding" below.
Findings from real hardware are recorded in
[`../testing/color-matrix-results.json`](../testing/color-matrix-results.json).
See [`../testing/README.md`](../testing/README.md) for how to run the probe
matrix end to end and record findings there.

## Why this exists

Arcen's audience for this work is colour graders and VFX artists. Three
properties matter to them, in this order:

1. **4:4:4 chroma.** 4:2:0 discards three quarters of the chroma samples. On UI
   text, node graphs, scopes and thin mattes that produces coloured fringing and
   makes an eyedropper read the wrong value. This is a correctness problem, not
   an aesthetic one.
2. **10-bit depth.** Not because desktops are 10-bit — they usually are not —
   but because two extra bits absorb the RGB→YCbCr rounding error, which makes
   the round trip *exact* for 8-bit sources. See "The 10-bit argument" below.
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
`range_changed` and `matrix_changed` alongside the existing fields.
`PlanDegradation::colour_degraded()` separates changes a colourist cares about
from an fps clamp they may not.

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
Performance authorizes host-ranked AV1 → HEVC → H.264 while preserving the
requested colour axes; Full Colour and Grading Reference request HEVC 4:4:4;
and an explicit variant remains an exact operator pin. Linux/L40S and
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

Both `NV_ENC_BUFFER_FORMAT_*_10BIT` and CoreVideo's `x`-prefixed formats store
samples **MSB-aligned** in a 16-bit word: a 10-bit code `v` is stored as
`v << 6`, giving `0xFFC0` for white, not `0x03FF`. Storing it unshifted
produces a picture four stops too dark. The shift is derived from the depth in
one place and pinned by test.

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

## Outstanding: the drawable is still 8-bit

Everything upstream now works: the host encodes 10-bit 4:4:4 full range, the
wire carries the colour on every frame, VideoToolbox decodes it, and a WGSL
shader converts it with the negotiated matrix and range. **The final stage is
still eight bits**, and it cannot be fixed from inside this crate:

- `wgpu-hal`'s `surface_capabilities()` only conditionally appends
  `Rgb10a2Unorm`, and only *after* `Bgra8Unorm`.
- `egui-wgpu` selects the first `Rgba8Unorm`/`Bgra8Unorm` match regardless of
  what else is offered.
- `wgpu-hal` then re-asserts `Bgra8Unorm` via `setPixelFormat:` on **every
  resize**, so even a successful override would not survive.

Verified directly against the vendored `egui-wgpu 0.35.0` source
(`src/lib.rs`, `preferred_framebuffer_format`, ~line 416): it iterates the
surface formats and returns the first `Rgba8Unorm` or `Bgra8Unorm`, and only
falls through to `formats.first()` when **neither** is present. `eframe`'s
`WgpuConfiguration` exposes `present_mode`, `desired_maximum_frame_latency`,
`wgpu_setup` and `on_surface_error` — none of which influences format choice.

That fallthrough is the precise unblock: if the Metal surface capabilities did
not advertise `Bgra8Unorm`, `egui-wgpu` would accept `Rgb10a2Unorm` unchanged.
So the smallest viable routes are, in increasing order of cost:

1. patch `wgpu-hal` to omit or reorder the 8-bit formats in
   `surface_capabilities()` (and stop the `setPixelFormat:` reassertion on
   resize); or
2. render video into its own `CAMetalLayer` outside `eframe`'s surface
   management, leaving egui on its 8-bit surface for UI only.

Option 2 is the more honest long-term shape — UI genuinely does not need
10 bits, and a dedicated video layer also removes egui's compositing from the
latency path — but both are dependency/architecture decisions rather than
implementation details.

What *is* done: the `CAMetalLayer`'s `colorspace` is set from the negotiated
`ColorPrimaries` (sRGB or Display P3), reached by raw `objc2` message sends —
the same technique `raw-window-metal` uses internally — with six typed
fail-safe outcomes logged once each rather than per frame. `pixelFormat` is
deliberately never touched, because `wgpu-hal` owns it.

Getting a true 10-bit drawable requires either a patched/forked `wgpu`, or
bypassing `eframe`'s surface management for the video layer. That is a
dependency and architecture decision, not an implementation detail, so it is
recorded here rather than worked around.

Until then: 10-bit is negotiated, encoded, transported, decoded and converted
correctly, and is then quantised to 8 bits at presentation. The colour
*accuracy* work (full range, correct matrix, exact 4:4:4) is fully realised;
the extra *depth* is not yet visible.
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

- **The drawable is still 8-bit.** The planar 10-bit frame reaches Deck's
  WGSL conversion correctly, but `egui-wgpu`/`wgpu-hal` still select and
  reassert `Bgra8Unorm`; use the two concrete unblock routes documented above.
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
