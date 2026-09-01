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

`RegionActivityScheduler` is the product adopter of that grid: one scheduler
per active region, composing `RegionActivityGrid` with Keel's `IdleCadence` so
no classification or keepalive algorithm is duplicated. Hosts feed it the
changed-tile summary the capture pipeline already computes for selective
conversion, so the adopter adds no extra hash pass. It returns a
`RegionServiceDecision` — serve or skip, keyframe or delta, a stable
`RegionServiceReason`, and an advisory `recommended_interval` — plus
fixed-field `RegionScheduleTelemetry`.

Suppression is bounded by construction. `RegionSchedulePolicy` validates
`frame_interval <= max_idle_refresh <= keyframe_interval`, and startup
baselines, `ForcedKeyframe` requests (client IDR, recovery, reconfigure),
keyframe deadlines, input/focus wake, and the max-idle refresh are all
mandatory reasons that measured activity can never override. Only sustained
idle backs the cadence off, and never past `deadline_remaining()`, so a static
region cannot outrun its own refresh. `note_service_failed` restores the
deadline counters and re-arms a lost keyframe rather than silently downgrading
it to a delta. Regions hold fully independent state, so one busy region cannot
starve another; cross-region delivery ordering stays with the existing
`arcen-outputs` fair roster and is not re-implemented here. Encoder backend,
codec/chroma, bitrate, and frame-rate policy remain host-authoritative — the
scheduler only decides whether a region is worth servicing this tick.

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

The crate remains `#![forbid(unsafe_code)]`, deterministic, I/O-free,
platform-free, and free of native handles. Wire messages, chunking, and
reassembly remain in `arcen-protocol`; AppKit, Win32, and X11 ownership remains
in product adapters.

## Video source and colour contracts

`arcen-media` is the shared boundary between native capture and native encode.
It does not choose WGC, DDA, NvFBC, XShm, CUDA, NVENC, VideoToolbox, or Metal.
It receives an explicitly typed source and owns the portable conversion and
truth that every platform must use.

| Source representation | Shared conversion |
| --- | --- |
| 8-bit BGRA | BGRA → NV12/I420/I444 for the negotiated matrix, range, and depth |
| Packed depth-30 RGB | Visual-mask-derived RGB10 → P010 or planar I444 P16 |
| FP16 linear scRGB, SDR contract | Clamp to SDR reference range → BT.709/sRGB OETF → planar I444 P16 |
| FP16 linear scRGB, HDR contract | Target-primary conversion → absolute ST 2084 PQ using 80-nit scRGB reference white → planar I444 P16 |

These functions are intentionally separate. A wider destination buffer is not
evidence of a wider source, and a ten-bit BT.709 stream is not HDR.
`VideoConfiguration` carries codec, chroma, depth, range, matrix, primaries,
and transfer as independent axes. `PlanDegradation` records every changed axis
plus fps, geometry, and cursor authority so platform adapters cannot hide a
fallback.

The production Deck's Auto/Speed/Grading/HDR choices resolve to complete
configurations before capture starts. Hosts then choose only a native provider
that can truthfully supply that configuration. Linux Xorg may change an HDR
request to Grading; a proven future Wayland provider may retain PQ. Windows
retains PQ only after exact-target HDR proof. Those platform decisions consume
the shared contract rather than redefining it.

## Video plan and portable software H.264

`video::{frame,convert,plan}` provides checked NV12/I420 views, allocation-free
BT.709 limited-range BGRA conversion, ordered backend resolution, typed
pre-READY unavailability, and strict READY/UNAVAILABLE v1 parsing. One
`ResolvedMediaPlan` supplies attachment codec, chroma, geometry, fps, cursor,
capability, hello, and frame-header truth.

The optional non-default `software-h264-source` feature is the only Arcen
OpenH264 boundary. `SoftwareH264Encoder` accepts checked borrowed I420, owns
bounded retained Annex-B output, enforces the reviewed Baseline/1080p30
contract, and exposes no native pointers. The dependency remains complex unsafe
C/C++; safe Arcen code does not make it memory-safe. The default graph has no
OpenH264 or native build dependency.

See
[`../../docs/architecture/media-plan-resolution.md`](../../docs/architecture/media-plan-resolution.md)
for host policy, source-only dependency, distribution, and physical-lab gates.

## Validation

`cargo test --locked -p arcen-media` and
`cargo clippy --locked -p arcen-media -- -D warnings` cover policy matrices,
UTF-8 boundaries, sequence/echo behavior, raster round trips, malformed inputs,
dimension/decoder/output limits, strict masks, row orientation, and capped
allocation behavior. On a Rust 1.89+ toolchain with C++ and NASM, also run
`cargo test --locked -p arcen-media --features software-h264-source` and
`cargo clippy --locked -p arcen-media --features software-h264-source --
-D warnings`.
