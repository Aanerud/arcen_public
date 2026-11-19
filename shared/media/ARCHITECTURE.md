# shared/media — `arcen-media`

`arcen-media` is an active, pure shared crate. It owns transport-independent
media/monitor contracts, the deterministic clipboard policy/raster core, fixed
audio policy, the first-tranche validated multi-monitor requested logical /
applied host-pixel topology and aggregate-plan contracts, and the shared
video-plan/frame/software-encoder core used by the active Piers.

`PerMonitorMediaPlan`/`AggregateMediaBudget`/`AggregateMediaPlan` (the
"aggregate-plan contracts" above) remain compiled, exported, and tested
exactly as shipped in the first tranche, but per
[ADR 0009's 2026-08-10 amendment](../../docs/adr/0009-multi-monitor-foundation.md)
they are superseded for encoder-capacity/admission gating by the measured
`shared/media/src/encoder_admission.rs` contract described below, which is
built on `RegionMediaPlan`/`RegionMediaRoster` rather than on those three
types. New capacity/admission work must use the measured contract; the ADR
amendment records the compatibility and removal policy for the superseded
types.

## Region domain

`RegionId` and `RegionGeneration` are validated nonzero identities, while
`OutputIdentity` is a nonempty bounded endpoint-local output identity.
`LogicalPoint`, `LogicalSize`, and `LogicalRect` use signed 1/120-logical-pixel
fixed point with checked, exclusive right/bottom bounds. `Scale120` is positive
presentation metadata with denominator 120, and
`OutputTransform` covers all four rotations and their reflected counterparts.

`RegionDescriptor` stores logical geometry, explicit pre-transform
`PhysicalSize` stream extent, scale, transform, output identity, and primary
truth. Physical stream size is not derived from scale: Deck presentation and
backing sizes may intentionally differ. `RegionSet`, `AppliedRegionDescriptor`,
and `AppliedRegionSet` enforce 1..=4 regions, unique region and output
identities, and exactly one primary. Applied origins may be negative.

`RegionActivityGrid` binds Keel's reusable 16x16 `ActivityGrid` to an exact
`RegionGeneration` and `RegionId`. It rejects stale, future, or wrong-region
updates before damage state changes, resets in place, and exposes fixed-size
activity diagnostics for aggregate scheduling. The cadence value is advisory;
it does not change encoder behavior or frame rates.

The software loop (`canvas::CanvasEncodeLoop`, below) is the one adopter of
Keel's damage and idle cadence on every Pier's software path; there is no
per-region service scheduler beside it.

For multi-monitor sessions, `SessionMonitorId` is a validated nonzero contract
(`1..=65535`). Wire/frame id `0` remains reserved for legacy single-monitor
video routing and is intentionally not constructible through the shared media
type.

`MediaStreamEpoch`, `RegionMediaPlan`, and `RegionMediaRoster` are the
host-authoritative per-region media contract. A bounded roster contains one
unique monitor id per entry and preserves each entry's own nonzero epoch,
encoder backend, codec/chroma, encoded size, fps, and
`BitrateBudgetKbps` bitrate budget. Consumers route and
validate frames against the matching entry; no first-monitor or session-global
wire profile is a valid substitute. Legacy single-monitor framing remains
unchanged.

`BitrateBudgetKbps` is the required, validated per-region bitrate value object
and the workspace's only nominal bitrate policy. It is nonzero and bounded to
`100..=500_000` kbps: below 100 kbps no codec/geometry pair this workspace
negotiates carries a usable desktop stream, and 500 Mbps is ten times the
planning ceiling, so both bounds reject mis-derived plans without constraining
real ones (four regions at the cap still sum inside `u32`).
`BitrateBudgetKbps::nominal_for_geometry` is the single pixel-rate-derived
planning heuristic — clamped to `NOMINAL_FLOOR_KBPS..=NOMINAL_CEILING_KBPS`
(`500..=50_000`), a strict subset of the invariant, so it is total and never
panics. Hosts populate `RegionMediaPlan::bitrate_budget` from it during
encoder planning/admission and publish
`RegionMediaPlan::applied_bitrate_kbps` verbatim as the applied wire media
plan's `bitrate_kbps`; Deck validates the received value back through
`BitrateBudgetKbps::new`. There is no per-host copy of this calculation. The
wire field's own type and invariant (`u32`, nonzero) are unchanged; the tighter
band is a media-domain invariant only.

Aggregate encoder admission pairs that roster with same-generation
`RegionActivityProfile` values and opaque exact platform binding IDs. One
candidate's complete encoder set is exercised concurrently through an
injectable `EncoderMeasurementAdapter`; the pure core computes per-region and
aggregate p50/p95 encode latency, p50/p95 queue age, delivered fps, and Jain
fairness normalized against offered frames. Thresholds have no shared default:
hosts supply measured policy values. Candidate zero may be accepted, a later
complete candidate may be selected as a host-authoritative reassignment, or the
whole roster is rejected atomically. Full-color profiles cannot be downgraded
below YUV444, and an adapter must open the exact binding token rather than
silently choosing another GPU/backend. Native probe implementations remain a
platform capability; deterministic fake adapters cover the shared contract.

`Monitor.width_px/height_px` retain each monitor's native pre-rotation mode
dimensions. `AppliedMonitor::desktop_rect_px` and
`AppliedMonitorTopology::desktop_bounds_px` derive the separate
rotation-aware on-desktop footprint, swapping width/height for 90/270-degree
placements without mutating the underlying mode dimensions.

`MediaContractError` is `#[non_exhaustive]`. Pre-1.0 callers must keep a
wildcard arm when matching it and treat added validation variants as Rust API
hardening, not as a wire change.

## Topology placement (`topology_placement.rs`)

`topology_placement` is the OS-free, product-free home for the multi-monitor
geometry primitives that Linux Pier, Windows Pier, and Deck previously
duplicated. It never inspects the platform; every behavioral difference is an
explicit caller-supplied input.

`TransformConvention` is the *required* rotation input. `NativeNeedsTransform`
means the caller's width/height are native pre-rotation mode dimensions that
this crate must rotate into an on-desktop footprint (both Piers, which report
RandR/`DEVMODE` mode extents); `AlreadyCompositorOriented` means the extent has
already absorbed the host transform and must not be rotated again, so the
emitted region transform is forced to `OutputTransform::Normal` (Deck, whose
multi-monitor-v1 stream arrives compositor oriented). There is no default and
no inference: passing the wrong convention is a visible, testable geometry
change, not a silent one.

`OriginPolicy` is the matching explicit desktop-origin input.
`TranslateToNonNegative` shifts a whole plan so its minimum corner lands on
`(0, 0)` (an Xorg/RandR screen has no negative space); `PreserveSigned` keeps
signed coordinates exactly as planned (the Windows virtual desktop and Deck's
applied roster both legitimately carry negative origins). Translation is
checked; an overflowing shift is rejected rather than wrapped.

`plan_edge_aware_offsets` is the shared edge-aware breadth-first placement:
starting from the primary, each monitor that shares a full touching edge with
an already-placed neighbor is converted using *that neighbor's* scale, so a
mixed-scale chain stays flush with no accumulated gap or overlap; monitors with
no touching edge fall back to the primary's scale. `place_monitors` composes
placement with the origin policy and returns a `PlacedLayout` carrying the
placed rects, the resulting bounds, and the applied translation.

`checked_layout_bounds` is unit-checked. Each input is a `SpacedLayoutRect`
tagging its origin and extent `LayoutSpace` (`LogicalArrangement` or
`HostPixel`), and a rect whose origin and extent disagree is rejected as
`MixedUnitRect`. This makes the ADR 0009 prohibition — never derive an
aggregate *physical* desktop extent from *logical* origins plus physical stream
sizes — a compile-adjacent runtime contract instead of a review convention;
`logical_origin_with_stream_extent_rect` exists only to name that forbidden
combination so tests can assert it is refused.

`scale120_from_scale` is the single fractional-scale→`Scale120` conversion for
all three callers, so no endpoint can drift on rounding or on the
non-finite/non-positive/out-of-range rejection boundary.

`build_region_sets` turns a `RegionPlacement` list into the paired requested
`RegionSet` and applied `AppliedRegionSet`, applying the declared
`TransformConvention` to each region's transform and cross-checking every
applied extent against the requested stream size. Input order is preserved.

`TopologyPlacementError` is `#[non_exhaustive]`; callers wrap it in their own
error type and keep a wildcard arm.

## Client applied-topology validation (`applied_topology.rs`)

`applied_topology` is the OS-free home for turning a host's applied
multi-monitor-v1 sidecar (`AppliedMonitorTopologyMsg`) into the exact facts a
*client* window/decoder plan needs, so Deck and the future Windows client share
one implementation instead of two.

`validate_applied_topology_for_production` checks, in this order: the
negotiated carrier is one the client implements, the topology generation is
nonzero, the roster is `1..=MAX_MULTI_MONITOR_COUNT`, and then per monitor in
primary-first order the display resolves, the `session_monitor_id` is a valid
nonzero `SessionMonitorId`, and the media plan reads back as a
`RegionMediaPlan` (epoch, encoder backend, codec, chroma, bitrate budget,
geometry/fps). The roster-level duplicate/size checks come last, from
`RegionMediaRoster`. The resulting `ValidatedAppliedTopology` is ordered with
the negotiated primary at index `0`.

`NativeDisplayResolver` is the single platform seam: it maps a wire
`ClientDisplayId` to the client's own native display handle
(`ValidatedAppliedTopology` is generic over it, so macOS gets a
`CGDirectDisplayID` and another client gets its own output identity). Any
`Fn(&ClientDisplayId) -> Option<T>` already implements it. Returning `None`
fails validation closed with `UnresolvedClientDisplayId`.

Applied rectangles are carried through verbatim in host pixels with signed
origins widened to `i64`: no `OriginPolicy` is re-applied client-side (the host
already published one coherent desktop rectangle, and re-translating it would
desynchronise the client from the host's own coordinates), and no rotation is
re-applied either — an applied extent is `AlreadyCompositorOriented` by
definition, so rotating it again would double-apply the transform.

`AppliedTopologyParts` is the same validation against already-distilled inputs,
so rosters a wire-validated message can never carry (empty, five monitors, a
duplicated monitor id) stay directly testable.

## Region frame admission (`region_frame.rs`)

`region_frame` is the pure "may this wire frame touch this monitor's decoder?"
decision, shared by every client that presents region video. It owns no queues,
decoders, textures, or UI.

`MonitorRoute` classifies a wire `VideoHeader.monitor_id`: `0` is
`LegacyPrimary` (today's single-monitor frame, which is *not* a negotiated
session monitor id), and `1..=65535` is `Negotiated(SessionMonitorId)`. The
distinction is type-level precisely because `SessionMonitorId` is nonzero by
construction.

`RegionFrameRoster` is the immutable committed fence: the admitted routes, each
route's `RegionMediaPlan`, and the `TopologyGeneration` the whole roster is
pinned to. `admit_frame` rejects in exactly this order — topology generation,
roster membership, stream epoch, codec/chroma profile — and `admit_route`
applies the first two to an already-decoded frame injected without a wire
header. A new topology means building a new roster, never mutating one in
place, so every method takes `&self`.

Keyframe/recovery fencing is deliberately *not* an admission error:
`region_frame_delivery` is a pure function of the caller's own per-region
recovery flag and the frame's keyframe status, returning `SkipUntilKeyframe`
(drop before the decoder, leave the cached frame and the recovery wait
untouched) or `Decode`. The per-region flag stays with the caller that owns the
decoder slot.

## Clipboard v1

- `ClipboardPolicy` enforces host-authoritative direction, content, and encoded
  transfer size. The product default is both directions, text and image, 8 MiB.
  Product configuration accepts 1–20 MiB; the shared constructor rejects zero
  and values above the 20 MiB protocol ceiling.
- Text is UTF-8 and truncates to a borrowed character-boundary prefix. Images
  are PNG on the wire and reject rather than truncate.
- `ClipboardSequenceGate` accepts only newer nonzero `u64` values.
  `EchoToken`/`EchoMarker`/`EchoSuppressor` prevent loops only; the token is not
  authentication and its debug representation is redacted.
- PNG decode is allocation-limited before the output buffer is reserved, rejects
  APNG, normalizes to 8-bit RGBA, caps decoded pixels at 64 MiB and either
  dimension at 8192, and validates the complete stream through IEND.
- DIBV5 conversion accepts exactly a 124-byte header, 32-bpp BI_RGB or standard
  BGRA bitfields, checked top-down/bottom-up rows, and sRGB. Output is top-down
  BGRA. PNG output uses a capped writer.
- `clipboard::formats` is the one host-format table: which X11 targets and
  MIME types carry text and PNG, the order a host reads them in (UTF-8 names,
  then `text/plain`/`TEXT`, then latin-1 `STRING`, then `image/png`), the
  names a host offers, latin-1 conversion, and `validate_payload` /
  `decode_host_payload`. Oversized payloads are refused, never truncated.
- `clipboard::bridge::SelectionBridge` is a pure event→action state machine
  for compositors with owner-change / read / set-selection / transfer
  clipboards (Mutter's `RemoteDesktop` today). It holds no clock and does no
  I/O. It does not send the baseline owner, never reads back its own
  ownership, and cancels an older read when a newer change arrives. A read is
  bounded at `max_bytes + 1`. At most `MAX_CONCURRENT_HOST_WRITES` writes run
  at once, and payloads are scrubbed.
- `clipboard::eligibility` decides whether a host session may carry the
  clipboard at all (authenticated, dedicated desktop, unprivileged user, a
  proven native bridge) and names the first failing fact.
- `ClipboardRequest::from_hello` and `policy_from_message` are the pure
  mappings between the client hello, the policy wire message and the shared
  types.

The crate remains `#![forbid(unsafe_code)]`, deterministic, I/O-free,
platform-free, and free of native handles. Wire messages, chunking, and
reassembly remain in `arcen-protocol`; AppKit, Win32, X11 and Mutter D-Bus
ownership remain in product adapters.

## Video source and colour contracts

`arcen-media` is the shared boundary between native capture and native encode.
It does not choose WGC, DDA, NvFBC, XShm, CUDA, NVENC, VideoToolbox, or Metal.
It receives an explicitly typed source and owns the portable conversion and
truth that every platform must use.

| Source representation | Shared conversion |
| --- | --- |
| 8-bit BGRA | BGRA → NV12/I420/I444 for the negotiated matrix, range, and depth |
| Packed depth-30 RGB | Visual-mask-derived RGB10 → P010 or planar I444 P16; SDR codes untouched (`Rgb10Signal::Direct`), a PQ desktop's SDR session converted on the served SDR curve (`PqBt2020ToSdrSrgb` for `srgb`, `PqBt2020ToSdrBt709` otherwise; `PqBt2020ToSdrGamma22`, stated `srgb`, when the capture evidence says the compositor decoded its SDR content with a 2.2 power, as Mutter does in HDR) |
| FP16 linear scRGB, SDR contract | Clamp to SDR reference range → served SDR transfer (sRGB keeps desktop codes; `sdr_desktop_transfer`/`serve_sdr_desktop` pick it for a capable client, BT.709 OETF otherwise) → planar I444 P16 |
| FP16 linear scRGB, HDR contract | Target-primary conversion → absolute ST 2084 PQ using 80-nit scRGB reference white → planar I444 P16 |

These functions are intentionally separate. A wider destination buffer is not
evidence of a wider source, and a ten-bit BT.709 stream is not HDR.
`VideoConfiguration` carries codec, chroma, depth, range, matrix, primaries,
and transfer as independent axes. `PlanDegradation` records every changed axis
plus fps, geometry, and cursor authority so platform adapters cannot hide a
fallback.

The production Deck's Auto/Speed/Grading/HDR choices resolve to complete
configurations before capture starts. Hosts then choose only a native provider
that can truthfully supply that configuration. Linux Xorg changes an HDR
request to Grading unless the operator declared the desktop Rec.2100 PQ
(`DesktopSignalEncoding`, `resolve_desktop_plan`); a proven future
Wayland provider may retain PQ. Windows
retains PQ only after exact-target HDR proof. Those platform decisions consume
the shared contract rather than redefining it.

### Colour check (`colour_check.rs`)

The colourist's checklist behind the Deck's **Colour check**. A platform
adapter gathers `HostFacts` (from the hello and `served_pipeline`),
`FrameFacts` (decoded frame headers), `DeckFacts` (display, presentation layer,
scale) and an `Exactness` reading; `assess` owns every rule, verdict, sentence,
stable `colour.*` code and one-click `ColourFix`, and `render_text` the
report. Fixes only change the Deck (preset, Retina-resolution streaming,
fullscreen) or measure; none changes a host setting. The capture source is
stated as implied by the served pipeline, because no host reports it.
`test_pattern::measure_biplanar` measures a decoded two-plane Y′CbCr picture
against a pattern in RGB and, for 4:4:4, in coded samples at the stream's
depth; `TestPattern::render_png` writes the pattern the host shows.

### Pipeline contracts

`shared/media/src/video/pipeline/` is the shared foundation for the four
product stream pipelines plus the software fallback. `mod.rs` owns the typed
contract surface (`PipelineId`, `PipelineContract`, `CodecPolicy`,
`BitratePolicy`, `KeelPolicy`) and the legacy inference helper for Decks that
do not send a wire pipeline. Each named file owns one contract:

- `auto.rs`: adaptive 8-bit 4:2:0 at the Deck display's refresh
  (`FrameRatePolicy::FollowDisplay`), detail-first, newest-frame present.
- `speed.rs`: adaptive 8-bit 4:2:0 at the display's refresh but never below
  60 fps, motion-first, one-frame buffer, newest-frame present,
  a safe link-capped start with a true 60 fps shape ceiling, and
  Speed-specific low-latency decode policy for clients that consume the
  served pipeline.
- `grading.rs`: HEVC 4:4:4 10-bit full-range BT.709 SDR, quality intent.
- `hdr.rs`: HEVC 4:4:4 10-bit full-range PQ/BT.2020, quality intent,
  conservative start, 500 Mbit/s ceiling, evidence-gated target-relative probe,
  and HDR-scoped queue budgets.
- `software.rs`: CPU fallback H.264 4:2:0 that follows the Deck display like Auto, at most OpenH264's 3840x2160 and 60 fps, held by the host's frame-rate governor at what the CPU encodes. It starts from a CPU-conscious shared bitrate policy (about 4 Mbit/s at 1080p30, scaled by picture shape) and lets the shared live rate controller climb toward an 8 Mbit/s-class ceiling only when the path proves clear. Speed is served visibly as Software/Auto at the CPU's rate; Grading and HDR are refused on software-only hosts. macOS Pier does not need this fallback because VideoToolbox is always present.

To change one pipeline, edit only its file and update the pinned tests in
`pipeline/mod.rs`; platform code should keep reading `pipeline_contract`.

`video/frame_rate.rs` resolves the served frame rate for every Pier:
`resolve_session_frame_rate` turns a contract's `FrameRatePolicy`, the Deck
display's refresh (`AuthResponse::display_refresh_hz`), the Deck's request
(a follow-display preset honours only a request below its legacy wire rate,
`PipelineContract::max_fps`, which Decks keep sending so older Piers serve
what they did) and the administrator's `video.fps` into a `SessionFrameRate`
with its `FrameRateBasis`; hosts narrow it to what their encoder and capture
prove (`SessionFrameRate::narrow`, `served_stream_rate`) and state it in the
hello (`stream_rate`) and `SESSION_STREAM_START` (`display_refresh_hz`,
`fps_basis`). The display refresh is the one the Deck states for its panel
(`AuthResponse::display_refresh_v1`), apart from the mode it asks the host to
create; a multi-monitor session resolves for its fastest display and
`monitor_frame_rate` lowers each monitor to its own, so a slow display never
caps a fast one. `served_head_refresh_hz` (`capture_rate.rs`) is the refresh
a host creates a head at for a served rate: twice the rate, else the rate,
else the Deck's requested mode, within what the host can create, never from
the Deck panel's refresh. A copying capture route without a rate measurement serves a
follow-display contract at most at `UNQUALIFIED_COPY_FPS` (capture admission).
`HostRateGovernor` judges a running Auto or Speed session on the host's own
encoded/delivered counts and steps it to an even divisor of the session rate
when the encoder falls behind, probing back up with backoff; the link's
Detail shedding (`RateController::detail_framerate`) runs under it.
Grading is the first fidelity contract with a fixed shared ceiling: it starts
at the previous link-capped value and may climb to 250 Mbit/s. Platform
encoders consume the paired `encoder_bitrate_bounds` so their native
`maxBitRate`/`DataRateLimits` ceilings do not clamp below the shared policy,
while VBV and average bitrate stay tied to the active target. The host adapters
carry the optional served-pipeline ceiling as data, not by matching colour
axes, so Custom streams cannot inherit a product ceiling by accident and later
pipelines can reuse the same mechanism. HDR reuses that same
`LinkCappedWithCeiling`/served-pipeline ceiling path at 500 Mbit/s rather than
adding an HDR-specific encoder-ceiling branch; its difference is the
evidence-gated probe step, which requires peak per-pipeline delivered evidence
before climbing.
To add a codec, add one `VideoCodec` variant with token/capability entries,
then add it to the relevant pipeline ladder. Auto/Speed codec ordering remains
data-driven (`AV1 -> HEVC -> H.264`) and `adaptive_codec_ladder` delegates to
that contract so callers do not hard-code the order.

The Deck's old per-preset colour table is now represented by the shared
pipeline contracts. Deck-local developer-only Custom/Exact axes remain outside
the four pipelines and do not send a `pipeline` token.

`server_hello.active_pipeline` is authoritative on hosts that know their final
encoder backend before hello. macOS creates the real VideoToolbox encoder only
after capture starts, so its hello value is provisional; it sends the additive
`served_pipeline` control message as soon as the session encoder's hardware /
software / unknown class is read back, and again after any encoder recreation.
Decks use the latest served-pipeline truth for the persistent degradation badge.

## Video plan and portable software H.264

`video::{frame,convert,plan}` provides checked NV12/I420 views, allocation-free
BT.709 limited-range BGRA conversion, ordered backend resolution, typed
pre-READY unavailability, and strict READY/UNAVAILABLE v1 parsing. One
`ResolvedMediaPlan` supplies attachment codec, chroma, geometry, fps, cursor,
capability, hello, and frame-header truth.

The optional non-default `software-h264-source` feature is the only Arcen
OpenH264 boundary. `SoftwareH264Encoder` accepts checked borrowed I420, owns
bounded retained Annex-B output, enforces Baseline within OpenH264's own
3840x2160 and 60 fps, cuts each picture into up to four fixed slices encoded on
OpenH264's worker threads, and exposes no native pointers. The dependency remains complex unsafe
C/C++; safe Arcen code does not make it memory-safe. The default graph has no
OpenH264 or native build dependency.

See
[`../../docs/architecture/media-plan-resolution.md`](../../docs/architecture/media-plan-resolution.md)
for host policy, source-only dependency, distribution, and physical-lab gates.

## Canvas planner and governor (ADR 0028)

`canvas::{state,pixels,mirror,tile}` is the one interpreter of canvas records
(the Pier's mirror and every Deck). `canvas::planner` decides what a Software
Pier sends, from Keel's facts only; `canvas::governor` decides how much of it
fits. Neither has an OS or codec dependency.

**Planner** (`CanvasPlanner`, one per monitor). Each tick, `observe` runs Keel
over the captured frame (narrowed hashing when the capture reports damage,
history, traits, motion search, classes, regions), checks every move hint
exactly against the mirror and returns the monitor's `MonitorDemand`; `plan`
spends the monitor's `MonitorGrant` and returns the wire records, already
applied to the mirror. Rules, first match wins:

1. A block wholly inside a verified move goes in a `Copy`. A move's source
   never holds lossy blocks (the longest exact band of rows is kept), and the
   band a copy scrolls through stays exact, so scrolling keeps copying.
2. A solid block goes in a `Fill` (coalesced into rectangles).
3. A whole 64 × 64 cell the Deck's cache holds is a `CachePaint`; a whole
   cell sent exactly is stored (`TileLedger` mirrors the Deck's slots and
   bytes, evicting first).
4. A `Motion` block inside a surface goes to that surface; elsewhere it is a
   tile.
5. Other changes are exact tiles; a compact many-colour band larger than
   `image_tile_limit_px` in one tick is one lossy picture of a one-shot
   surface.
6. `Settled` lossy blocks are refined to exact tiles with leftover budget.
7. Motion over 60 % of the monitor for 0.5 s is one whole-monitor surface
   (today's full-frame stream as a degenerate case).

Surfaces (`SurfaceSet`): a surface opens only with blocks to paint and paints
its IDR in the same tick; it keeps its rectangle while its motion fits and
covers more than half of it, moves without an IDR when only the position
changed, and reopens on any other change or a new scale. The Pier implements
`SurfaceEncoder` (the shared software H.264 session; a later backend behind
the same trait).

**Governor** (`CanvasGovernor`, one per session). The budget is
`tick × logical cores × video.software.cpu_share` (`CpuShare`, 50 % by
default), kept as credit so a picture due every Nth tick may spend N ticks'
budget. Spending order: mandatory work (a reset and its repaint, a new
surface's IDR, damage within 64 px of the latest input, keepalives) always;
then copies, fills and cache paints; then exact tiles and one-shot pictures,
oldest first; then surfaces at the governed rate; then refinement on monitors
without surface work. Across monitors the focused monitor goes first, then
the one with most motion. Degradation order: the surface rate through the
session rate's even divisors (`HostRateGovernor`), then surface quality
(bitrate 75 %, 50 %; minimum QP +4, +8), then surface scale (⅔, ½), and tiles
spread over more ticks only when exact work alone exceeds the budget.
Recovery runs in reverse after the shared probing holds. `canvas_ack` flow
control (`CanvasFlow`): at most two unacknowledged frames per monitor carry
optional work (surfaces, refinement), and none while the Deck holds more
unapplied bytes than the link carries in 250 ms; mandatory and exact work
never wait for an acknowledgement. Costs are `CostModel` predictions over
Keel's `WorkMeter`, with priors until the meter has measured.

Gates (`canvas/planner_tests.rs`, `canvas/governor.rs`): corpus replays over
every Keel scenario (mirror digest equals an independent replay of the wire
bytes every frame; command mix and CPU estimate), a budget simulation on the
`WorkMeter` cost model (mandatory work never deferred; text p95 latency at
most one tick under a saturating video; one budget across monitors), and a
deterministic test for each degradation step.

**The one software loop** (`canvas::CanvasEncodeLoop`). Every Pier's software
capture (Linux Xorg `XShm`, Linux Wayland `PipeWire`, Windows WGC) is a thin
adapter: it hands each picture and whatever damage its OS reported to
`tick` and writes the records that come back; `wants_frame` tells it when a
picture is owed (damage, waiting work, a keyframe or reset, the keepalive).
The decision is per Deck and made once, by the shared
`canvas::serve::plan_canvas`: a Deck that offered `canvas_v1` to a
single-monitor Software session gets canvas records (planner → governor →
`SurfaceEncoder` → records, monitor 0 with topology generation and stream
epoch 0); any other Deck gets the same loop's degenerate path, the whole
monitor as one surface encoded by the same encoder
(`LoopEncoder::encode_full`) and sent as ordinary access units, gated by
Keel's damage and idle cadence. Hardware contracts never reach it. The Pier
implements the encoder once, `video::SoftwareSurfaces` (one shared
`SoftwareH264Session` per surface, BT.709 limited range, plus the
monitor-sized session of the full-frame path), and opens the loop with
`video::open_software_loop`. The decision travels to `capenc` as one
argument (`canvas::serve::CanvasCapencArgs`), the records come back as
framed-v2 `Canvas` records, and the Deck's `canvas_ack` and
`canvas_reset_request` go to the loop as `CANVAS-ACK`/`CANVAS-RESET` lines
(`capenc_control`). A Pier that drops a canvas record asks the loop for a
keyframe, which starts a new generation (reason `recovery`); a reset is the
queue's and the lane relay's recovery point. The loop logs the mirror's
digest at every fence that is a multiple of `CANVAS_DIGEST_EVERY_FENCES`;
`examples/canvas_verify.rs` is a Deck for one capture (the CPU executor,
acknowledgements, the same digest fences) for lab comparison.

## Validation

`cargo test --locked -p arcen-media` and
`cargo clippy --locked -p arcen-media -- -D warnings` cover policy matrices,
UTF-8 boundaries, sequence/echo behavior, raster round trips, malformed inputs,
dimension/decoder/output limits, strict masks, row orientation, and capped
allocation behavior. On a Rust 1.89+ toolchain with C++ and NASM, also run
`cargo test --locked -p arcen-media --features software-h264-source` and
`cargo clippy --locked -p arcen-media --features software-h264-source --
-D warnings`.
