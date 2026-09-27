# How the macOS Pier streams, and what that was measured to cost

This records what was measured rather than what was assumed, because most of
the time spent on the first slow-stream investigation went into theories that
turned out to be wrong. Where something is still unproven, it says so.

## Frame rate, not the encoder, is the latency lever

Nine hypotheses about the encoder were eliminated before anyone tried simply
asking for more frames. Measured through a real Deck against an unmodified
host, motion arm, changing only the requested rate:

| | 30 fps | 60 fps |
| --- | --- | --- |
| delivered | 29 | **57** |
| source interval | 34.1 ms | **17.4 ms** |
| encode | 12.93 ms | 11.91 ms |
| `wire_age_ms` on the Deck | 27 | **6** |
| `decode_ms` | 2.5 | 1.85 |
| queue / send | 0.06 / 0.00 | 0.05 / 0.00 |

Roughly **59 ms of measured pipeline becomes 27 ms**, at 7.4 Mbps with no
backlog and no drops. Most of the gain is not the encoder at all: halving the
source interval halves how long a change waits to be photographed, which is
~8 ms on its own and dwarfs anything the encoder was going to give.

`--max-fps 60` does this today. The Deck defaults to `PerformanceMode::Standard`,
which is 30; `High` is already 60.

### Nine things that did not work

Each was measured on the lab, and each is worth knowing precisely because it
looked promising:

| tried | result |
| --- | --- |
| halving the pixels (1080p) | 12.66 vs 12.84 — 44% fewer pixels bought 1.4% |
| HEVC instead of H.264 | 16.46 — worse |
| H.264 Main instead of High | 13.04 — no change |
| `MaxFrameDelayCount = 0` | 12.80 — no change, though the default is *unlimited* |
| `EnableLowLatencyRateControl` | 13.7 — accepted, and worse |
| `PrioritizeEncodingSpeedOverQuality` | accepted, no change |
| `ExpectedFrameRate` 120 | 12.98 — no change |
| launchd `ProcessType: Interactive` | 12.95 — no change |
| our own per-frame `CVPixelBuffer` | 0.015 ms — never us |

The invariance to pixel count is the clue that should have redirected the
search sooner: **a cost that does not move when the work halves is not
compute.** Published reports put VideoToolbox on Apple Silicon at 10–15 ms
with an intrinsic pipeline delay, and 12.8 ms sits in that band. It is a
floor, not a bug.

## The encoder spends its average, whatever the picture does

VideoToolbox does not treat `AverageBitRate` as a budget it draws on when the
content needs it. It spends it. Measured on the lab against a real Deck:

| | bytes per frame |
| --- | --- |
| desktop running an animation | 34.9 KB |
| desktop with nothing moving | 34.8 KB |
| the configured average ÷ fps | 34.5 KB |

Halving the target halved the bytes, to 17.1 and 17.2. Content did not enter
into it at any point.

Two things follow. A target above what the link carries is not an upper bound
rarely reached — it is a permanent backlog, every frame, forever. And idle
bandwidth cannot be reclaimed by suppressing unchanged frames alone, because
the frames that *are* sent each cost the full average regardless.

At 2560×1440 the shared arithmetic asks for 8.29 Mbps, and this link answered
with a 52 ms writer queue and 22 fps of a requested 30. Capping the figure at
what the same arithmetic spends on 1080p of the same cadence:

| | motion | static |
| --- | --- | --- |
| fps | 22 → **29** | 22–28 → **29** |
| `mean_queue_ms` | 52.20 → **0.07** | 29.91 → **0.05** |
| `mean_send_ms` | 24.78 → **0.00** | 15.46 → **0.00** |

## What a still desktop actually costs — and a retraction

**An earlier version of this file claimed a still desktop cost the full
bitrate and that ScreenCaptureKit reports 99.83% damage every frame. Both
were measured on a desktop that was not actually still.**

On a confirmed-idle lab desktop — Safari quit, no foreground application,
nothing animating — measured through a real Deck:

| | frames in 25 s | bandwidth |
| --- | --- | --- |
| 30 fps | 28 | **0.09 Mbps** |
| 60 fps | 53 | **0.1 Mbps** |

`frames_suppressed` is zero in both, which is the point: nothing needed
suppressing, because **ScreenCaptureKit stopped delivering**. It is
damage-driven, and idle genuinely costs almost nothing.

Two conclusions that were drawn from the bad measurements are therefore
withdrawn:

- that idle bandwidth was a problem worth engineering against — it is about
  a hundredth of what was claimed;
- that `dirtyRects` are useless because they always cover the whole screen.
  The 99.83% figure came from a desktop with something moving on it. A later
  census on an idle desktop read 47.77%. Whether damage is a usable signal
  here is **open again**, and needs measuring on a desktop confirmed idle
  before anything is built on the answer.

The lesson is cheap to state and was expensive to learn: **"I quit the
browser" is not the same as "the desktop is idle"**, and the difference was
a factor of two hundred in bandwidth. Confirm the state you think you are
measuring.

## The host was never asked for thirty

The longest-running wrong theory in this file was that ScreenCaptureKit could
not deliver faster than 67 ms. It delivered exactly what it was asked for.

`StreamProfile::default()` on the Deck requested `max_fps: 15` — half the
lowest preset the client offers — and `--connect`, the headless form README
documents, never overrode it. Neither the saved `performance_mode` nor an
explicit `--max-fps 30` changed the value on the wire. So the Deck asked for
15, the Pier served 15, and ScreenCaptureKit paced at 1/15 s = 66.7 ms.

Everything downstream was consistent with a host at its ceiling, because it
was at the ceiling it had been given. Measured on the lab with a real Deck at
2560×1440:

| | before | after |
| --- | --- | --- |
| `mean_source_interval_ms` | 67.39 | **34.08** |
| negotiated fps | 15 | **30** |

Three explanations were tried first and are wrong. Each is recorded because
each looked convincing:

- **The consumer was never behind.** `LatestFrame::superseded` had never once
  been non-zero across every session in the lab log. The encode loop was
  always already waiting when a frame arrived.
- **The capture callback was never the cost.** It occupies 0.013 ms mean and
  0.064 ms at worst against a 67 ms interval — 0.02% of the budget.
- **The arranged display's refresh rate was not the cause.** Raising it from
  `fps` to `2 × fps` left the interval at 67.37 ms, unchanged to two decimals.
  The change was kept, because building a display at exactly the rate you
  intend to capture leaves no headroom, but it fixed nothing.

### Measure the source, not the wait

`mean_capture_wait_ms` cannot tell a source offering frames slowly from a
consumer taking them slowly, and those need opposite fixes. `SourceCadence`
measures the gap between ScreenCaptureKit's own delivery callbacks, and the
time spent inside them. When `mean_source_interval_ms` and
`mean_capture_wait_ms` agree, the ceiling is the source's and no amount of
consumer tuning will move it — which is what they did, to two decimals, for
as long as this was misdiagnosed.

It reports every 150 frames rather than at teardown. The producer is joined
with a timeout and abandoned if it misses one, so an end-of-run summary is the
one record a wedged capture will never write.

## The shape of the pipeline

```
ScreenCaptureKit  ──►  capture thread  ──►  one-slot latest-wins handoff
                                                      │
                                              encode thread
                                          (Keel decides; VideoToolbox encodes)
                                                      │
                                              bounded queue
                                                      │
                                          session task ──► QUIC writer
```

Three stages, three owners. That matters more than any one of them being fast.

## What was measured

All figures below are from an Apple Silicon development Mac unless stated.

### VideoToolbox already uses hardware

Read back from a live session with
`kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder`:

| Session | Hardware |
| --- | --- |
| H.264 64×64 | yes |
| H.264 1920×1080 | yes |
| HEVC 1920×1080 | yes |
| H.264 3600×2338 | yes |

Hardware encoding has been allowed by default since macOS 10.15, so **not**
setting `RequireHardware...` proves nothing either way, and neither does
setting `EnableHardware...`. Only the read-back is evidence. The host now logs
which encoder it got, as `encoder_backend`, because the first question anyone
asks about a slow stream is which one it was.

A standing theory that the slow stream was software encoding is therefore
retired. It was never true.

### The stages used to run in sequence

A recorded session reported means of 24.10 ms waiting for a frame, 15.39 ms
encoding and 25.84 ms sending, and delivered 15.34 fps.

```
1000 / (24.10 + 15.39 + 25.84) = 15.31 fps   predicted if the stages add up
                                 15.34 fps   actually delivered
1000 / max(24.10, 15.39, 25.84) = 38.70 fps  predicted if they overlap
```

Stages that add up to the period within 0.2% are not merely slow, they are
serialised. That is what the capture/encode split fixed.

### VideoToolbox overlaps submissions

`encode_frame_async` takes `&self`, so a session can hold several frames at
once; the old code moved the session out of the struct for the duration of
each wait, which made that impossible by construction.

| Path | Per frame | Throughput |
| --- | --- | --- |
| Submit and wait, one at a time | 11.3 ms | 88.8 fps |
| Two in flight | 5.2 ms | 191.7 fps |
| Four in flight | — | 171.0 fps |
| Eight in flight | — | 185.2 fps |

Depth two captures the whole gain; deeper buys nothing, so the encoder's
effective pipeline depth is two. Note that 88.8 fps already clears a 30 fps
target — **encode was never the blocker**, which is why the current code still
submits one at a time and spends its complexity elsewhere.

### Holding capture surfaces stops capture

A probe that tried to hold 60 surfaces from the eight-surface pool stopped
receiving frames altogether, and `dropped_frames` stayed at **zero** the whole
time. Starving the pool stops delivery upstream of the counter that exists to
notice loss, so a low frame rate with no drops recorded is not evidence that
nothing was lost.

This is why the handoff between capture and encode holds exactly one frame and
replaces it rather than queueing.

## What each fix was worth

| Change | Was | Is |
| --- | --- | --- |
| Capture and encode overlap | stages add to the period | period is the slowest stage |
| Audio drain bound | up to ~100 packets per turn, ahead of video, on the same writer | 8, oldest dropped and counted |
| Keel damage | every captured frame encoded and sent | unchanged frames suppressed; keepalive at 1 s |
| Requested geometry | host's own screen size, whatever the Deck asked | the size the Deck asked for |
| Bitrate | flat 20 Mbps | derived; 4.67 Mbps at 1080p30 8-bit 4:2:0 |

The geometry one is easy to underrate. A 1920×1080 Deck in front of a
3600×2338 host was being sent 3600×2338 — four times the pixels to encode and
push, for a picture the Deck then resampled anyway.

## What the local test suite cannot tell you

**Sending over loopback costs about 0.05 ms per frame.** The recorded lab
session spent 25.84 ms. The condition that makes backpressure matter does not
exist on a development machine, and `a_thirty_fps_session_delivers_near_thirty_fps`
passed both before and after the stages were split. It is a floor that would
catch a regression into something much worse, not evidence about backpressure.

**A development machine has no idle desktop.** The terminal running the suite
is itself animating the screen. `a_still_desktop_sends_almost_nothing` measured
`suppressed=0` at 9.1 Mbps on a desktop nobody was touching, because the test
output was the motion. It reports rather than fails in that case, and the
policy it cannot always exercise is covered deterministically in
`damage_policy_tests` instead.

Anything about frame rate under real network conditions has to be measured
against a real Deck over a real link.

## Things that are true and easy to get wrong

- **`dirtyRects` is genuinely delivered.** Measured 20 frames out of 20
  carrying rectangles, 146 in total, none unknown. It is the union of what was
  redrawn and moved, in pixels, from macOS 12.3.
- **Being told nothing is not being told nothing changed.** A missing or
  unparsable damage attachment marks the whole frame. An empty list is the
  compositor saying the desktop is still. Collapsing the two is a bug someone
  has to find from a bandwidth graph.
- **Damage rectangles round outward.** A rectangle covering part of a pixel
  changed that pixel.
- **Damage accumulates across frames that were not sent**, and the cadence is
  committed only once a frame is actually on its way. Clearing either at the
  moment of decision loses regions a failed send was carrying.
- **Inter prediction already makes unchanged regions cheap.** Full-picture
  H.264 does not re-encode static pixels as new data. Damage guides
  *scheduling*; it does not become partial access units, and dirty rectangles
  cannot be sent as independent payloads to an unchanged decoder.
- **Input bounds are the host desktop, not the picture.** Pointer coordinates
  arrive normalised and are mapped onto the desktop the pointer moves across.
  These were the same number only for as long as the host ignored the Deck's
  requested size.
- **Bit depth does not imply HDR**, and a ten-bit container proves neither HDR
  nor a genuinely ten-bit source.

## Consult the reference before concluding something is impossible

`refference/osx/Files_interest` holds a shipping commercial macOS remote
desktop agent. It may be read and analysed, never copied. Twice now a
conclusion of "macOS cannot do this" has been reached by probing, and the
reference has settled it in one command.

Reading a binary's imports says what a working product actually calls:

```sh
nm -u <binary> | grep -i cursor
strings <binary> | grep -i cursor
```

For the cursor that answered, in order: `currentSystemCursor`, `image` and
`hotSpot` are read; `kCGDisplayStreamShowCursor` exists for the composited
mode; `CursorShapeSharedMemoryPool` and "cursor shape's bitmap data" say the
bitmap is carried; "cursor polling resumed"/"suspended" say it is polled rather
than subscribed to; and a settings key naming a "locally rendered cursor" says the client
draws it locally by default, with an option to stop. That is a complete design,
recovered without reading a line of anyone's source.

Notably it imports **no private cursor API** — no `CGSCurrentCursorSeed`, no
`CGSGetGlobalCursorData`. What it uses is public.

## Two ways a cursor probe can lie

Both were made here, and both produced a confident wrong answer.

**Warping is not moving.** `CGWarpMouseCursorPosition` relocates the pointer
without posting an event, so no application processes a mouse-moved event and
none of them has any reason to change the cursor. A probe built on warping
alone measures its own omission. The session path warps *and* posts; a probe
must do the same.

**An unmatched image is not an arrow.** Reporting "no known cursor matched" as
`CursorShapeKind::Default` makes it indistinguishable from the cursor really
being an arrow. Count distinct images separately from named shapes: images
climbing while shapes stay flat means the matching is broken, which is a
different fault from the cursor never changing.

## A headless Mac has one display mode, and that decides the geometry story

Measured on the lab Mac Studio with `probe-modes`, asking for scaled and
duplicate-resolution modes and reading pixel rather than point sizes:

```
display 12 currently 1920x1080
  1920x1080
```

One mode. No physical monitor is attached, so macOS synthesises a single
1920x1080 framebuffer and offers nothing else. `CGDisplaySetDisplayMode` cannot
give a Deck the 2560x1440 desktop it asked for, because there is nothing to set
it to — and a Deck with a larger screen therefore sees a 1080p desktop scaled
up, which is exactly as soft as that sounds.

The reference solves this with virtual displays. Its own strings say so —
"Default system virtual display is not active", "it is not a \<vendor\> virtual
display", "virtual display id {} serial number mismatch" — and its imports name
the mechanism: `CGVirtualDisplay`, `CGVirtualDisplayDescriptor`,
`CGVirtualDisplayMode` and `CGVirtualDisplaySettings`. Those are **private**
CoreGraphics classes. They appear in no public SDK header.

So the options are a private API that a shipping commercial product depends on,
or a desktop that is whatever size the host's framebuffer happens to be. That
is a decision about what this product is willing to depend on, not a technical
question, and it should be made deliberately rather than discovered.

Note what this also means for a headless host: there is no physical console to
disturb, so the usual objection to changing a host's display configuration does
not apply here.

## Check the log level before trusting a diagnosis

Every measurement above was taken with the host at `logging.level: 0`, which is
`OperationalProfile::Critical` and an `EnvFilter` of `warn,arcen=error`. No
debug record had ever been written: the audio-backlog line did not appear in a
session that dropped 530 packets.

```sh
grep -A 2 logging "/Library/Application Support/Arcen/pier.json"   # host
grep -o '"log_level":[0-9]*' ~/Library/Application\ Support/Arcen/settings.json  # Deck
```

`level` is the cumulative profile: 0 critical, 1 error, 2 info, 3 debug. Both
ends need 3 before an end-to-end question is worth asking.

Note the per-record `profile_name` field is **not** the filter. It is the level
that record requires, so a log full of `"critical"` records says nothing about
what is being filtered out.

## Do not run `python3` on the lab machine

It raises the Command Line Tools installer on its console, the same way `xcrun`
does. Use `grep`, `sed` and `awk` there, and parse the output locally.

## A faster source does not help a slow writer

Measured, in both directions. `minimumFrameInterval` genuinely is a floor, so
setting it to exactly the session period genuinely does cap delivery below the
requested rate. Lowering it still made everything worse:

| | before | after |
| --- | --- | --- |
| capture wait | 40.02 ms | 42.42 ms |
| encode | 23.42 ms | 24.14 ms |
| queue | 10.96 ms | **71.96 ms** |
| send | 4.42 ms | **26.33 ms** |
| delivered | 20.60 fps | 15.17 fps |
| audio dropped | 45 | 530 |

Capture was never the binding constraint. Queue and send were already the
largest numbers in the session, and both are writer-side, so producing frames
faster only gave a writer that was already behind more to be behind on.

Read the largest number first, and check which side of the pipeline it is on.

## Four pipelines, each its own contract

Auto, Speed, Grading and HDR are separate capture-to-encode chains, not one
loop with switches. All figures here are from the lab over its VPN link, with
the Deck GUI on another Mac.

### Read the bitstream, not the plan

Each keyframe's SPS is logged as `encoded stream truth` on the Pier and
`received stream truth` on the Deck (`arcen_media::hevc_sps`). The Deck also
warns when the SPS contradicts the frame header.

This was not cosmetic. Every Grading session this host served was HEVC Main
4:2:0 8-bit, untagged, captured as `420v`, while the plan and the header said
4:4:4 10-bit:
- the capability probe claimed no 10-bit or 4:4:4, for want of evidence;
- the encoder never set a profile, and VideoToolbox handed no profile encodes
  Main;
- the Deck's decoder converted whatever arrived into the format it was asked
  for.

### VideoToolbox facts, measured

- `kVTProfileLevel_HEVC_Main44410_AutoLevel` is exported but not declared in
  the SDK headers. It is looked up with `dlsym`. With it, an `xf44` surface
  encodes RExt 4:4:4 10-bit, and the colour tags land in the VUI.
- Main 10's SPS states `general_profile_idc` 1 with only the Main 10
  compatibility flag set, over genuinely 10-bit samples. Depth and chroma
  are what to check.
- Real-time sessions write no HDR10 SEI (mastering display, content light
  level). That holds from session properties and from frame attachments,
  for Main 10 and for Main 4:4:4 10.

### The link has a knee, and the bill is per second

VideoToolbox spends its target whatever the picture does, so the target is
the link bill. HEVC 4:4:4 10-bit, 30 fps, frame age p95:
- 4 Mbps: 5 ms;
- 6 Mbps: 7, 18, 35, 385 and 505 ms on five runs;
- 8 Mbps: 590 ms;
- 10.8 Mbps (the shared sizing): 441 ms, with the frame rate falling to 19.

Grading therefore gets the fast path's own 1080p budget (4.6 Mbps). At 60
fps, the frame-rate-scaled budget gave Speed 42 fps and a 670 ms p95. Capped
at the 30 fps bill, the same content ran at 57.5 fps with an 11–20 ms p95.

### HDR is proven, then normalised

- **The panel.** A virtual display whose mode has transfer function 1 is an
  HDR panel: 5× potential headroom, HDR mode enabled. Values 0 and 2–6 are
  SDR.
- **The proof.** SkyLight's potential headroom matches `NSScreen`'s: 16.0
  for a built-in XDR panel, 5.0 for this panel, 1.0 for SDR. HDR is claimed
  only above 1.0.
- **What ScreenCaptureKit delivers.** Asked for ITU-R 2100 PQ and the BT.2020
  matrix, the `xf44` surfaces carry exactly those tags. Apple's own preset
  asks for Display P3 PQ and a BT.709 matrix.
- **Where SDR white lands.** In ScreenCaptureKit's PQ output SDR white sits
  near 100 nits, for the local and canonical references alike, and the
  Deck's HDR desktop looked dimmer than Grading. A Core Image stage
  multiplies linear light by 2.03, which puts white at BT.2408's 203 nits
  (`arcen_media::video::pq_white`). It must run in an autorelease pool:
  without one its images pinned the capture surfaces, and capture stopped
  after eight frames.

### Measure with content that moves at the rate you are testing

A locked lab screen animates at about 30 Hz. Speed measured there looks like
a 30 fps pipeline. A 60 Hz cursor sweep composited by the host
(`--cursor-mode host`) is a 60 Hz source that touches no Arcen input path.

## Not done

- The host's own display mode is unchanged, so "match my primary display"
  gets a correctly sized *picture*, not a client-sized *desktop*. Windows and
  text keep their physical size and the Deck sees the whole desktop scaled to
  fit. Changing host resolution needs a display-mode transaction and separate
  review.
- No Metal preparation path for SDR. HDR has a Core Image stage; the direct
  `420v` fast path costs nothing today and stays untouched.
- On-screen fidelity proofs need an unlocked lab screen: the `chroma_detail`
  pattern exactness run, and HDR pass-through of an HDR image (the luminance
  census should peak near 1000 nits).
- Multi-in-flight VideoToolbox submission is measured but not adopted, because
  encode is not the bottleneck.
- No qualification against a real Deck over a real link.
