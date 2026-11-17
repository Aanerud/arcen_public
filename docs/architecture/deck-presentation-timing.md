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
| Auto (and Software, custom, unknown) | `Newest` | Present each frame the moment it is decoded, without tearing: the newest frame wins at the next refresh or composition; an older one still waiting there is discarded. |
| Speed | `Tearing` | As `Newest`, and a frame may also replace the one being scanned out (a tear) where the display path allows it: speed over the last 1 % of picture quality. |
| Grading, HDR | `Vsync` | Wait for the refresh. Every frame is shown whole and none is skipped: a fidelity contract shows exactly what was encoded. |

`PresentTiming::for_depth` keeps any ten-bit picture on `Vsync`, whatever
the stream was served as. The encoder side already matches: Auto and Speed
are `EncodeIntent::Interactive`, which NVENC encodes with preset P4 and
`NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY` (no lookahead, no B-frames;
`hosts/capenc/src/nvenc.rs`, [encode intent](encode-intent.md)); Grading and
HDR are `Quality` (P6, `HIGH_QUALITY`, still no reordering).

## Windows Deck (DXGI flip model)

The video swapchain is a flip-model composition swapchain
(`DXGI_SWAP_EFFECT_FLIP_DISCARD`, two buffers, waitable, maximum frame
latency 1, created with `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING` where DXGI
reports support).

| Timing | `Present` |
| --- | --- |
| `Vsync` | sync interval 1, no flags |
| `Newest` | sync interval 0, no flags |
| `Tearing` | sync interval 0, `DXGI_PRESENT_ALLOW_TEARING` when supported |

For the flip model, sync interval 0 means "cancel the remaining time on the
previously presented frame and discard this frame if a newer frame is
queued" ([`IDXGISwapChain::Present`](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-present)):
mailbox behaviour without tearing. `DXGI_PRESENT_ALLOW_TEARING` is valid
only with sync interval 0 and a swapchain created with the tearing flag
([variable refresh rate displays](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/variable-refresh-rate-displays)).
A frame can tear only where DWM flips the swapchain directly (independent
flip); while DWM composes, it is shown whole at the next composition.

## macOS Deck (CAMetalLayer)

The eight-bit layer (`BGRA8Unorm`, Auto and Speed) and the ten-bit layer
(`RGB10A2Unorm`, Grading and HDR) are separate `CAMetalLayer`s.

| Timing | Eight-bit layer |
| --- | --- |
| `Vsync` | rendered on a `CVDisplayLink` tick from a FIFO pacer (up to three frames), `displaySyncEnabled` on, `present(_:afterMinimumDuration:)` half a refresh, two drawables in flight |
| `Newest` | rendered the moment the frame arrives; the pacer keeps only the newest; one drawable in flight (Core Animation shows display-synced drawables in order, so a second one would queue behind the first); `displaySyncEnabled` on; the display-link tick is the retry path when the drawable is busy |
| `Tearing` | as `Newest`, with `displaySyncEnabled` off, two drawables in flight (without display sync a drawable is shown as soon as it is ready, so the second does not wait behind the first) and `present(_:)` without a minimum duration |

`displaySyncEnabled`: "If false, the layer presents new content more
quickly, but possibly with brief visual artifacts (tearing)"
([CAMetalLayer.displaySyncEnabled](https://developer.apple.com/documentation/quartzcore/cametallayer/displaysyncenabled)).
`maximumDrawableCount` stays 3 (only 2 or 3 are allowed,
[maximumDrawableCount](https://developer.apple.com/documentation/quartzcore/cametallayer/maximumdrawablecount));
the in-flight gate, not the pool size, keeps one drawable pending.
`presentsWithTransaction` stays off. The ten-bit layer is unchanged:
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
