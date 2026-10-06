# Four separate stream pipelines, chosen by the Deck

Plan, October 2026. Every "today" statement cites the code at `main`
(`c8b3df7`); nothing here is taken from another document.

Update on `feat/pipeline-grading`: Grading keeps the previous link-capped
starting bitrate and now has a shared 250 Mbit/s ceiling. The host rate
controllers consume the shared contract, and native encoder ceilings are raised
through the generic served-pipeline ceiling path (`NVENC maxBitRate` on Linux
and Windows, `VideoToolbox DataRateLimits` on macOS). VBV remains sized from
the active target, not from the ceiling.

## Intent

Auto, Speed, Grading and HDR are four separate pipelines for four kinds of
work, not one capture loop with switches:

| Pipeline | For | Promise |
| --- | --- | --- |
| **Auto** | everyday desktop work | sharp, adaptive, light on link and battery |
| **Speed** | video editors, 3D artists | lowest latency, up to 60 fps, sharpness gives way first |
| **Grading** | VFX compositors, colourists | 10-bit 4:4:4 BT.709 SDR, exact colour |
| **HDR** | HDR grading and review | true 10-bit PQ/BT.2020, conservative start with a 500 Mbit/s evidence-gated ceiling |

A **software fallback** serves a host with no usable GPU encoder, typically a
Proxmox or VMware VM that wants something better than RDP. It is the last
resort, not a fifth preset.

Three rules hold for all of them:

1. **The Deck decides.** The Deck names the pipeline in the authenticated
   setup request; the host serves that pipeline or says exactly what it served
   instead. The host never silently picks a different pipeline.
2. **Shared first.** Each pipeline is a contract in `shared/` (selection,
   colour, bitrate, cadence, keel policy, degradation). Platform code only
   captures, encodes and presents.
3. **Keel everywhere.** No pipeline spends bandwidth or Deck power on pixels
   that did not change: idle cadence on every host encoder, and presentation
   on the Deck only when a new frame arrives.

## How the Deck asks today

1. The Deck UI has the four presets plus `Custom`
   (`clients/macos/src/ui/app.rs:1022`). A preset is stored as two settings, a
   performance mode and a colour fidelity
   (`StreamingPreset::apply_to`, `app.rs:1052`): Auto = Standard/Standard,
   Speed = High/Standard, Grading = Standard/GradingReference,
   HDR = Standard/Hdr10.
2. Colour fidelity becomes the wire's `VideoSelectionIntent`
   (`ColorFidelitySettings::video_selection`, `app.rs:969`): **Auto and Speed
   both send `AdaptivePerformance`; Grading and HDR both send
   `ColorFidelity`**. `max_fps` and `motion_priority` come from the preset
   contract (`app.rs:10657-10673`).
3. The request travels as `InitialVideoRequestMsg`
   (`shared/protocol/src/messages.rs:587`, `video_selection` at `:604`) inside
   the authenticated `AuthResponse.initial_video` (`messages.rs:889`), so the
   host builds the real encoder before it says hello.
4. The host resolves it with shared code. `AdaptivePerformance` lets the host
   choose the codec from AV1 → HEVC → H.264
   (`shared/media/src/video/intent.rs:74-100`,
   `shared/media/src/video/policy.rs:283-297`). That is why a Deck asking for
   "h264" under Auto received AV1 from a GeForce RTX 4080. An administrator
   pin can override or refuse the request (`policy.rs:289-292`, `:325-347`).
5. The host answers in `ServerHelloMsg` (`messages.rs:1816`) with `active_*`
   colour axes, and the Deck presents what the host says arrived, not what it
   asked for (`ActiveContract::from_hello`,
   `clients/macos/src/ui/media_worker.rs:424-434`).

**Gap:** the wire never names the pipeline. The host cannot tell Auto from
Speed, or Grading from HDR, except by inferring from fields such as `max_fps`,
`motion_priority` and transfer. A pipeline can therefore not own its own
bitrate, keel or degradation policy on the host, because the host does not
know which pipeline it is running.

## The matrix today

Columns are hosts, plus the Deck's receiving side. ✅ = implemented,
⚠️ = implemented with a gap, ❌ = not implemented or refused.

### Auto: 8-bit 4:2:0, ≤30 fps, 2-frame buffer (`shared/media/src/video/preset.rs:81-92`)

| Stage | Linux Pier | Windows Pier | macOS Pier | Deck |
| --- | --- | --- | --- | --- |
| Capture | ✅ NvFBC → CUDA, device to device (`hosts/capenc/src/linux.rs:1277-1318`) | ✅ DXGI Desktop Duplication after a real frame is proven, else WGC BGRA8 (`hosts/capenc/src/win.rs:705-817`) | ✅ ScreenCaptureKit `420v` (`hosts/macos/src/capture.rs:222-224`) | — |
| Encoder | ✅ NVENC, AV1/HEVC/H.264 by adaptive ladder | ✅ NVENC, AV1/HEVC/H.264 by adaptive ladder | ✅ VideoToolbox H.264/HEVC; AV1 refused, Apple has no AV1 encoder (`hosts/macos/src/session.rs:1799-1805`) | ✅ VideoToolbox decode, AV1 in hardware on M3 and later |
| Bitrate | ⚠️ starts at the 1080p30 8-bit budget, 4.7 Mbit/s, and climbs to the shape formula (`hosts/linux/src/net/server.rs:6323-6334`, `shared/media/src/video/bitrate.rs:55-113`) | ⚠️ same (`hosts/windows/src/session.rs:5699-5712`) | ⚠️ shared formula; the call that applies it to VideoToolbox is unverified | — |
| Keel | ✅ `SubmissionGate` idle cadence; QP map built, off by default (`hosts/capenc/src/qp_map.rs:74`) | ✅ `SubmissionGate` (`hosts/capenc/src/win.rs:1300`), merged in #159; QP map off | ✅ ScreenCaptureKit dirty rects → `ExternalDamage` + `IdleCadence` (`hosts/macos/src/stream.rs:2895-2900`) | ❌ no damage awareness; every UI pass repaints the window |
| Presentation | — | — | — | ⚠️ VideoToolbox frame copied to CPU memory (`clients/macos/src/pipeline/video_decoder.rs:3006-3140`), uploaded again (`clients/macos/src/ui/video_render.rs:1048`), drawn by the egui pass; presents without vsync (`app.rs:16089`) |

### Speed: 8-bit 4:2:0, ≤60 fps, 1-frame buffer, motion first (`preset.rs:93-103`)

Same capture, encoder and keel as Auto on every host. Differences:

| Stage | Linux Pier | Windows Pier | macOS Pier | Deck |
| --- | --- | --- | --- | --- |
| Bitrate | ⚠️ start still billed at 30 fps (`bitrate.rs:92-113`); motion priority reaches the rate controller (`server.rs:6335-6344`) | ⚠️ same | ⚠️ same, application unverified | — |
| Presentation | — | — | — | ⚠️ same path as Auto; at 60 fps with an active pointer the Deck measured 120–564 window passes per second, ~60% CPU plus ~62% in WindowServer |

### Grading: 10-bit 4:4:4 BT.709 SDR, ≤30 fps, 8-frame buffer, quality encode (`preset.rs:105-115`)

| Stage | Linux Pier | Windows Pier | macOS Pier | Deck |
| --- | --- | --- | --- | --- |
| Capture | ✅ depth-30 Xorg, MIT-SHM RGB10; never NvFBC (`hosts/capenc/src/linux.rs:1484-1518`) | ✅ WGC FP16 scRGB; Desktop Duplication cannot (`hosts/capenc/src/win.rs:726-737`, `hosts/capenc/src/wgc.rs:71-112`) | ✅ ScreenCaptureKit `xf44`, 10-bit 4:4:4 (`hosts/macos/src/capture.rs:413-431`) | — |
| Conversion | ✅ RGB10 → CUDA upload; PQ desktop → SDR via the shared plan (`resolve_desktop_plan`, `shared/media/src/video/desktop_encoding.rs:106-177`) | ✅ scRGB → SDR BT.709 in shared code (`convert_scrgb_to_sdr_i444_p16`, `shared/media/src/video/convert.rs:1310`) | ✅ native | — |
| Encoder | ✅ NVENC, 10-bit 4:4:4 where probed | ✅ NVENC, 10-bit 4:4:4 where probed | ✅ VideoToolbox HEVC Main 4:4:4 10, SPS-proven (`hosts/macos/src/encode.rs:88-121`, `:1516-1555`) | ✅ VideoToolbox |
| Bitrate | ✅ starts at the previous 8-bit 1080p30 budget and may climb to the shared 250 Mbit/s Grading ceiling; NVENC `maxBitRate` carries the ceiling while VBV follows the active target | ✅ same | ✅ same; VideoToolbox `AverageBitRate` starts at the shared start and `DataRateLimits` carries 250 Mbit/s | — |
| Keel | ✅ `SubmissionGate` | ✅ `SubmissionGate` | ✅ `ExternalDamage` + `IdleCadence` | ❌ |
| Presentation | — | — | — | ✅ dedicated RGB10A2 `CAMetalLayer`, zero copy through `CVMetalTextureCache` (`clients/macos/src/ui/video_metal_layer.rs`); the Deck media worker now feeds a presenter-thread latest-frame mailbox instead of driving Grading/HDR presentation from egui passes |

### HDR: 10-bit PQ/BT.2020, ≤30 fps, 8-frame buffer (`preset.rs:117-127`)

| Stage | Linux Pier | Windows Pier | macOS Pier | Deck |
| --- | --- | --- | --- | --- |
| Source proof | ⚠️ only when the operator declares a Rec.2100 PQ desktop; otherwise falls back to Grading (`desktop_encoding.rs:106-177`) | ✅ HDR EDID/topology, exact target, `RGB_FULL_G2084_NONE_P2020` (`hosts/capenc/src/win.rs:1011`) | ✅ headroom > 1.0 on an HDR panel (`hosts/macos/src/session.rs:1650-1671`) | — |
| Conversion | ✅ direct PQ, or the RGB10 pipe (`linux.rs:1923-2005`) | ✅ scRGB → BT.2020/PQ (`convert_scrgb_to_pq_i444_p16`, `convert.rs:1253`) | ✅ Core Image 203-nit white (`hosts/macos/src/hdr_white.rs:250-303`) | — |
| Bitrate | ✅ safe shared start, 500 Mbit/s HDR ceiling through the generic served-pipeline ceiling path, evidence-gated fast probe; NVENC starts at true session fps | ✅ same | ✅ safe start plus VideoToolbox `DataRateLimits` ceiling and HDR-scoped writer queue | ✅ HDR-scoped media inbox |
| Presentation | — | — | — | ✅ same 10-bit layer; EDR on only for PQ (`video_metal_layer.rs:774`) |

### Software fallback (VMs, no GPU encoder)

| Stage | Linux Pier | Windows Pier | macOS Pier |
| --- | --- | --- | --- |
| Path | ✅ X11 software capture → CPU BGRA→I420 → OpenH264 (`hosts/capenc/src/linux_x11.rs:1039-1084`) | ✅ Desktop Duplication/WGC → CPU conversion → Media Foundation H.264 or OpenH264 (`hosts/capenc/src/win_mf.rs`, `hosts/capenc/src/mf_encoder.rs:17-34`) | ❌ none; VideoToolbox only |
| Serves | ⚠️ Auto only: H.264 8-bit 4:2:0, local cursor, 1–30 fps; Speed, Grading and HDR refused (`linux_x11.rs:1014-1036`) | ⚠️ Auto only, H.264/NV12 | — |
| Bitrate | ✅ shared Software contract start/ceiling plus runtime `BITRATE` control into OpenH264 (`linux_x11.rs`) | ✅ shared Software contract start/ceiling plus runtime updates into MF/OpenH264 (`win_mf.rs`, `mf_encoder.rs`) | — |
| Keel | ✅ shared region scheduler (`hosts/capenc/src/region_schedule.rs`, `linux_x11.rs:1091-1125`) | ✅ shared region scheduler (`win_mf.rs:924`) | — |

## Action points

Shared work comes first because every platform consumes it.

### S1. Name the pipeline on the wire (`shared/protocol`, `shared/media`)

Status 2026-10-03: foundation implemented on `feat/pipelines-foundation`.
`InitialVideoRequestMsg.pipeline` and `ServerHelloMsg.active_pipeline` carry
the four product tokens (`auto`, `speed`, `grading`, `hdr`) with unknown future
tokens preserved during deserialisation so a new peer does not break the whole
message. Custom/developer exact axes still omit the field.

- Add a `pipeline` field (Auto, Speed, Grading, HDR) to
  `InitialVideoRequestMsg`, sent by the Deck from `StreamingPreset`.
- Echo the served pipeline in `ServerHelloMsg` and the READY line. A
  difference becomes a typed, visible degradation, never a silent change.
- Keep `Custom` and the developer overrides as `Exact`, outside the four
  pipelines.
- Compatibility: a host that does not know the field keeps today's inference.
  Escalate to Shared/Architecture: protocol change.

### S2. One shared contract per pipeline (`shared/media/src/video/preset.rs`)

Status 2026-10-03: foundation implemented in
`shared/media/src/video/pipeline/`. Each pipeline has its own file and complete
data contract; `video::preset::{StreamingPreset, PresetContract, contract}`
delegates to that module for compatibility. Host adapters read bitrate bounds
from the contract without changing today's numbers; follow-up pipeline agents
can now edit their own pipeline file.

Extend `PresetContract` so each pipeline owns its whole policy, not just fps
and buffer:

- codec ladder (Auto/Speed: AV1 → HEVC → H.264; Grading/HDR: HEVC 4:4:4 10, or
  AV1 10-bit 4:2:0 where 4:4:4 is impossible, decision pending);
- bitrate start and ceiling. Replace the single `link_capped_average_bitrate_bps`
  start with a per-pipeline policy: Grading sized for fidelity, HDR with no
  fixed ceiling;
- keel policy: idle cadence always; QP map per pipeline (proposed on for
  Auto/Speed, off for Grading/HDR so quality stays uniform);
- degradation order (what each pipeline gives up first).

Hosts then read the contract by pipeline name instead of re-deriving it.

### S3. Keel on every host encoder

- ✅ Linux NVENC (both paths), Windows NVENC (#159), macOS Pier, both software
  paths.
- ✅ Linux and Windows software fallbacks use the shared Software bitrate contract and accept runtime bitrate updates from the shared rate controller.
- Decide the QP-map default per pipeline (S2) and enable it from the contract.

### D1. Deck: a dedicated 8-bit presentation path for Auto and Speed (`clients/macos`)

Modelled on how the reference client presents (analysed, not copied): video
draws in its own renderer and layer, separate from the UI toolkit.

- Branch `feat/pipeline-auto` implements the root/single-window path first:
  VideoToolbox requests IOSurface/Metal-compatible NV12 buffers, the media
  worker presents fresh 8-bit frames through a separate display-synchronised
  `CAMetalLayer`, and secondary monitor viewports intentionally stay on the
  existing egui/wgpu upload path until they get their own layers.
- A dedicated 8-bit `CAMetalLayer`, separate from the 10-bit layer
  (`clients/macos/AGENTS.md` forbids routing Auto/Speed through the wide
  layer).
- Zero copy: wrap the VideoToolbox `CVPixelBuffer`'s IOSurface as a Metal
  texture; delete the CPU copy and re-upload on this path.
- Draw once per new decoded frame, paced to the display's refresh, with the
  pointer as a small overlay that moves without redrawing the picture.
- egui repaints only when its own UI changes.
- Interim, already prototyped on local branch `wip/deck-root-display-sync`:
  pace the root layer to the display refresh in single-window sessions.

### D2. Deck: Grading/HDR presentation independent of egui

Status 2026-10-04: implemented on `feat/pipeline-grading-d2`. The Deck shares
D1's presenter-thread/latest-frame mailbox lifecycle for native Grading/HDR
frames while keeping the colour-specific path explicit: Grading presents native
`xf44` HEVC Main 4:4:4 10, BT.709 SDR through RGB10A2 Metal with EDR off; HDR
presents PQ/BT.2020 through the same RGB10A2 layer with EDR/HDR10 metadata only
when the host confirms PQ. The existing egui/wgpu upload path remains the
visible fallback if layer setup or asynchronous rendering fails.

### H1. Software fallback as a real VM pipeline

- Linux and Windows: keep Auto (H.264 8-bit, ≤30 fps) as the software contract. Speed degrades visibly to Software/Auto because a 60 fps CPU encode path would spend the VM's scarce CPU before proving a benefit.
- macOS Pier: none today. A macOS VM has VideoToolbox, so there is no software fallback to select.
- Grading and HDR stay hardware-only and refuse visibly in a VM.

## Open decisions

1. HDR bitrate ceiling: resolved at 500 Mbit/s, using the generic `LinkCappedWithCeiling` contract.
2. Grading bitrate ceiling.
3. Whether the high Grading/HDR budgets apply on LAN only, or everywhere with
   the rate controller adapting to the link.
4. Order: D1 first (fixes Deck heat), or S1/S2 first (the contracts).
