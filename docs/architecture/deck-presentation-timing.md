# Deck presentation timing

**Status.** Implemented on the macOS Deck and the Windows Deck (2026-11-15).
Measurements: see the end of this page.

A Deck shows each decoded frame on a schedule that belongs to the stream's
pipeline contract, not to the platform. The rule lives in shared code
(`arcen_media::video::PresentTiming`, a field of every `PipelineContract`);
the shared media worker reads it from the pipeline the host actually serves
(`operational_present_timing`: its `served_pipeline` message, else the
hello) and hands it to the presenter with every frame
(`arcen_deck_core::present::PresentableFrame::timing`). A presenter maps it
onto its display API and nothing else.

| Pipeline | Timing | Meaning |
| --- | --- | --- |
| Auto, Speed (and Software, custom, unknown) | `Newest` | Present each frame the moment it is decoded, without tearing: the newest frame wins at the next refresh or composition; an older one still waiting there is discarded. |
| Grading, HDR | `Vsync` | Wait for the refresh. Every frame is shown whole and none is skipped: a fidelity contract shows exactly what was encoded. |

No preset tears. Speed presented with tearing until 2026-11-18; since then
Speed keeps its low latency (60 fps floor, 1-frame encoder buffer, motion
kept on a weak link) without tearing. It cost about 3 ms on
average (Mac fullscreen 7.3 vs 4.3 ms, table below). The `Tearing` timing
and both presenters' tearing paths were then removed. The measurements
below that mention Speed's tearing are history.

`PresentTiming::for_depth` keeps any ten-bit picture on `Vsync`, whatever
the stream was served as. The encoder side already matches: Auto and Speed
are `EncodeIntent::Interactive`, which NVENC encodes with preset P4 and
`NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY` (no lookahead, no B-frames;
`hosts/capenc/src/nvenc.rs`, [encode intent](encode-intent.md)); Grading and
HDR are `Quality` (P6, `HIGH_QUALITY`, still no reordering).

## Windows Deck (DXGI flip model)

The video swapchain is a flip-model composition swapchain
(`DXGI_SWAP_EFFECT_FLIP_DISCARD`, two buffers, waitable, maximum frame
latency 1, created without `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING`). Each window's presenter thread waits for the swapchain
holding no lock (also before its first frame), takes the newest frame,
draws it under its device's immediate-context lock, and calls `Present`
outside that lock. The decoder and the presenters log where their time
goes every five seconds (`decoder timing` per decoder, `decoded picture
hand-over timing`, `video presenter timing` per window and pipeline).

**Decode never waits for presentation.** The decoder runs on a D3D11
device of its own on the root window's adapter, and every window presents
on a device of its own on that adapter
(`clients/windows/src/decode/shared.rs`, `present/handover.rs`). The
decoder copies each picture it outputs into one of a bounded pool of
NT-handle shared textures (never a shared decoder array, which some
drivers corrupt) and signals a shared fence; the presenter opens the
texture and fence on its device, waits GPU-side for the fence before it
samples (decode → render), and signals a fence of its own after its
draws, which the decoder waits for GPU-side before it writes that texture
again (render → decode). Nothing waits on the CPU; a full pool drops the
newest picture rather than waiting. The split is used only where the
shared rule `arcen_deck_core::decode::decode_device_split` qualifies the
driver (extended resource sharing, shareable fences; Intel, NVIDIA from
driver 471.11, AMD from Vega: shared decode textures are known to corrupt
on pre-Vega AMD, older NVIDIA, Qualcomm and Moore
Threads drivers) and, per output format, a real keyframe handed over reads
back sample-exact on the presenter's device. On one virtual GPU,
NV12 and P010 do and `Y410` does not (it opens, but reads differently), so
there a connection that asks for 4:4:4 (Grading, HDR) decodes on the
window's device as before; Auto and Speed get the split. Everything else
falls back to one device for the decoder and every window, as before.

Before the split (2026-11-18 field report, two 2560x1600 monitors): on the
lab's virtual GPU with two windows on two displays, `Present` itself took
15-16 ms (1-2 ms with a single window) while holding the shared device's
own multithread lock, which held every decode call for about 11 ms and the
other window's draws, so the two monitors decoded about 60 frames a
second between them (55 + 25 shown). A third buffer, a larger device frame
latency and `DXGI_PRESENT_DO_NOT_WAIT` changed nothing. Measured after the
split: see the end of this page.

| Timing | `Present` |
| --- | --- |
| `Vsync` | sync interval 1, no flags |
| `Newest` | sync interval 0, no flags |

For the flip model, sync interval 0 means "cancel the remaining time on the
previously presented frame and discard this frame if a newer frame is
queued" ([`IDXGISwapChain::Present`](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-present)):
mailbox behaviour without tearing. Without `DXGI_PRESENT_ALLOW_TEARING`
(and a swapchain created with the tearing flag,
[variable refresh rate displays](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/variable-refresh-rate-displays))
no present tears, whether DWM composes it or flips the swapchain directly.

## macOS Deck (CAMetalLayer)

The eight-bit layer (`BGRA8Unorm`, Auto and Speed) and the ten-bit layer
(`RGB10A2Unorm`, Grading and HDR) are separate `CAMetalLayer`s.

| Timing | Eight-bit layer |
| --- | --- |
| `Vsync` | rendered on a `CVDisplayLink` tick from a FIFO pacer (up to three frames), `displaySyncEnabled` on, `present(_:afterMinimumDuration:)` half a refresh, two drawables in flight |
| `Newest` | rendered the moment the frame arrives; the pacer keeps only the newest; `displaySyncEnabled` on; one drawable in flight where a picture reaches the glass within a refresh, two where the system composes the layer and it measurably takes longer (`arcen_media::NewestPresentDepth`, below); a frame that finds the slots busy is rendered the moment a drawable's presented handler frees one (the presenter's slot thread), the display-link tick stays the retry path |

`displaySyncEnabled` stays on for every timing: "If false, the layer
presents new content more quickly, but possibly with brief visual
artifacts (tearing)"
([CAMetalLayer.displaySyncEnabled](https://developer.apple.com/documentation/quartzcore/cametallayer/displaysyncenabled)).
`maximumDrawableCount` stays 3 (only 2 or 3 are allowed,
[maximumDrawableCount](https://developer.apple.com/documentation/quartzcore/cametallayer/maximumdrawablecount));
the in-flight gate, not the pool size, keeps one drawable pending.
`presentsWithTransaction` stays off.

**How many pictures `Newest` keeps in flight.** The slot is released by
the drawable's presented handler, so one picture in flight shows at most
one picture per round trip from submission to the glass. On a full-screen
layer scanned out directly that round trip is under a refresh. Where the
system composes the layer first (a window, a scaled display mode) it is
two to three refreshes: Apple notes the latency "may be bigger if the
system needs more time, such as for windowed modes on macOS"
([CAMetalDisplayLink.preferredFrameLatency](https://developer.apple.com/documentation/quartzcore/cametaldisplaylink/preferredframelatency)).
One picture in flight then shows every second or third refresh: the owner's
5K-scaled 60 Hz display showed 20 of 40-48 frames a second. The presenter
measures the round trip of pictures submitted while none other was in
flight and keeps a second one in flight once its median exceeds a refresh
(withdrawn below three quarters of one); the shared rule is
`arcen_media::NewestPresentDepth`. Two in flight with a three-drawable pool
leaves one drawable for the picture on the glass, so `nextDrawable` never
waits. Log: `eight-bit video layer pictures in flight follow the display
path`, and `submit_to_glass_p50_us`/`max_in_flight` in the per-second
`dedicated presenter pipeline` line. The ten-bit layer is unchanged:
`CVDisplayLink` pacing, display sync on, EDR and the PQ colour space only
when the host returns PQ.

## Fullscreen is pixel for pixel

In fullscreen nothing is drawn over the video, for every preset, on both
Decks. The Windows Deck's connection bar opens only when the pointer
reaches the top-edge strip above it or Ctrl+Alt+Home pins it; the window's
input lets the UI see the pointer only there
(`arcen_deck_core::input::overlay_pointer`). The macOS Deck has no bar: the
system menu bar auto-hides and drops down at the top edge.

With the default Displays mode (Match layout) and Single display, the
stream is pinned to the primary monitor's own pixel size
(`StreamSizingPolicy::PinPrimary`), so a fullscreen picture is 1:1 (Deck
pixel = host pixel). On a Retina Mac this holds with HiDPI streaming on;
with it off, the stream is the point size and Core Animation scales it 2×.
Windowed mode follows the window on the macOS Deck (`display_update`);
the Windows Deck streams at the monitor's size in every mode and scales the
picture into a smaller window, so it never asks the host to resize
(follow-up: wire the shared `DisplayFitTracker`).

## Measurements

Lab, 2026-11-15, against a Linux X11 Pier serving a moving test page.

**Windows Deck**, on a virtual machine with a GRID vGPU whose desktop DWM
always composes (no independent flip, so no tearing is possible there),
60 Hz. PresentMon, video swapchain, present to display. "Swept" delays each
present by a 0–16.7 ms sawtooth (a lab-only build) so every phase of the
refresh is sampled; an unswept run is one phase.

| Preset | Before (sync interval 1) | After | Shown |
| --- | --- | --- | --- |
| Auto, swept, windowed / fullscreen | 26.6 / 26.7 ms | **12.0 / 12.7 ms** (interval 0, no tearing) | 100 / 98 % |
| Auto, unswept, windowed / fullscreen | 29.5 / 29.7 ms (one phase) | **12.2 / 12.7 ms** | 100 % |
| Speed, swept | 20.5 / 20.3 ms (interval 0 + tearing, unchanged) | 19.7 / 18.9 ms | 98 % |
| Grading | refused on that machine (its display adapter has no HEVC 4:4:4 10-bit decode), as before | | |

**macOS Deck**, MacBook with a 120 Hz panel. The Deck's own telemetry:
frame hand-off to the presenter until the drawable's `presentedTime`,
median per run, three to four runs per cell (each run is one phase sample).

| Preset | Window | Before | After | Frames replaced unseen |
| --- | --- | --- | --- | --- |
| Auto | fullscreen | 12.7 ms | **7.3 ms** | 0 |
| Auto | windowed | 22.7 ms | **20.5 ms** | 0 |
| Speed | fullscreen | 12.8 ms | **4.3 ms** | 0 |
| Speed | windowed | 24.2 ms | **17.2 ms** | 0 (17 % with one drawable in flight, hence two) |
| Grading | fullscreen | — | 13.0 ms, ten-bit layer, EDR off (unchanged path) | 0 |

**macOS Deck, composed 60 Hz display** (2026-11-18; MacBook with a DELL
U2723QE in its 5K-backed "looks like 2560x1440" mode, Auto, a Linux X11
Pier serving a 60 fps animation; one 35-40 s run per cell; "shown" is
confirmed presents over frames received):

| Window | Before (one in flight) | After (two once measured) |
| --- | --- | --- |
| windowed | 27.4 of 59.7 fps shown (46 %), 34 ms to glass | 38.7 of 60.2 (64 %), 54 ms; the round trip alone is three refreshes (49 ms), so two in flight cap it at 40 |
| fullscreen | 29.1 of 60.1 (48 %), 34 ms | 56.2 of 59.4 (95 %), 37 ms |

**Windows Deck, separate decode device** (2026-11-18; the same GRID vGPU
virtual machine, two 2560x1600 windows on two displays with Match my
layout, Auto, against a Linux X11 Pier serving a 60 fps animation on each
of its two monitors; the Deck's five-second `decoder timing` and `video
presenter timing` lines, medians while moving; one decoder per monitor):

| | Before (one device) | After (own decode device, a device per window) |
| --- | --- | --- |
| Two windows: presented fps per window | 26 / 26 | **59.9 / 59.8** |
| Two windows: decoded fps per monitor | 27.3 / 27.2 | **59.9 / 59.9** |
| Two windows: decode call | 13.6 ms average (lock wait 4.6 ms) | **0.26 ms** (no lock wait); hand-over copy 0.16 ms |
| Two windows: `Present` (root / other window) | 15.7 ms each, holding the device | 14.0-14.9 / 4.0-5.4 ms, holding nothing the decoder or the other window needs |
| Two windows: skip-ahead to a keyframe in 3 min | 211 | **0** |
| One window: presented / decoded fps | 60 / 60 | 60 / 60 |
| One window: decode call | 2.8 ms average, 16.9 ms worst | **0.26 ms**, 0.7 ms worst |

The vGPU's `Present` still blocks for most of a refresh in the root
window; it now costs nothing else. Two swapchain buffers do not starve
with frame latency 1 there: five buffers (a lab-only build) gave the same
60 / 60 fps, the same `Present` and frame-latency waits, and the same
8-9 frames replaced unseen in 2 minutes. AMD, where another client found
three buffers starving, is not in the lab.

## Several monitors: one timeline

In a multi-monitor session each monitor is its own stream, decoded on its
own. Before a decoded frame reaches its presenter, the shared media worker
passes it through the session's timeline
([ADR 0026](../adr/0026-one-timeline-for-every-monitor.md),
`arcen_media::timeline`, `arcen_deck_core::runtime::frame_sync`): a frame
of a faster stream waits until the slowest active stream's frames
captured at the same time are ready too, at most 50 ms, and a lone active
stream never waits. The worker logs the measured skew every five seconds
(`multi-monitor timeline`); `ARCEN_DECK_TIMELINE=off` measures without
holding. The presenter's own refresh phase (above) comes after it.

After the timeline, each further window of the macOS Deck shows at most
one new picture per refresh of its own display
(`arcen_media::RefreshPacedSlot`): a frame that arrives early waits for
the refresh and a newer one replaces it, but none is thrown away. Capture
is change-driven, so a monitor whose desktop is still sends nothing for
seconds; the last frame of a burst must reach the screen, and a picture
that has not changed is not a stalled stream
(`arcen_deck_core::video_stall`: a stall is a keyframe wait, a monitor
whose packets keep arriving without reaching the screen, or a host that
stopped answering).

Every Pier stamps all of its monitors on one clock, the wire clock, so the
timeline works the same whichever Pier serves the session. Where the stamp
is taken differs, and only shifts the absolute latency, never the skew
between streams of the same Pier:

| Pier | Stamp taken |
| --- | --- |
| Linux | When the access unit is read from the capture-encode helper (`arcen-capenc`), one helper per monitor |
| Windows | When the access unit is read from the capture-encode helper, one helper per monitor |
| macOS | When VideoToolbox returns the access unit, before it is framed and sent |
