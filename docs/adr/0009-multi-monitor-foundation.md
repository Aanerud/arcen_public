# ADR 0009: Multi-Monitor Foundation

**Status:** Accepted (2026-08-06) — foundation-only shared contracts and
documentation; no product rollout or behaviour change yet. Amended
2026-08-10: the `AggregateMediaPlan`/`AggregateMediaBudget`/
`PerMonitorMediaPlan` contract from this ADR's original tranche is superseded,
for encoder-capacity/admission gating, by the measured encoder-admission
contract in `shared/media/src/encoder_admission.rs`; see "Amendment
(2026-08-10)" below. No public Rust API changed as part of the amendment.

## Context

Arcen Deck, Pier-Linux, and Pier-Windows currently behave as primary-display
products even when the client can enumerate more than one display. The approved
multi-monitor plan needs a frozen shared contract before any Deck or host
implementation can safely change product behaviour.

This ADR records only the first approved tranche: shared topology/admission
types, additive protocol-v3 negotiation fields, and the measurement gate that
must exist before carrier selection or release claims. It is not evidence that
any current product can yet stream more than one monitor.

## Decision

### Product behaviour to freeze now

- Match My Layout v1 targets **all active client displays**, bounded to **1..=4**
  monitors. There is no subset picker or manual reorder in the first release.
- Admission is **atomic**. Arcen either proves and serves the full requested
  topology or fails Match My Layout with a reconnect path. It must never
  silently serve a subset.
- The topology is **fixed for the attachment**. Local display add/remove,
  rearrangement, rotation, or scale changes require reconnect.
- Deck multi-monitor presentation remains **native fullscreen per display** and
  requires macOS **Displays have Separate Spaces**. Failure of that preflight
  blocks authentication with exact operator guidance.
- Capture, Keel cadence/state, encode, queueing, decode, and presentation are
  **independent per monitor**. Multi-monitor is not a stitched-canvas feature.
- Admission is **two phase**: admit/dry-run first, then bind/verify against the
  real outputs and encoders, and commit only when every monitor is ready.
- Rollback must preserve a **non-headless invariant**. Exact restore is the
  target; if exact restore fails, the host must land on a verified safe-primary
  topology rather than zero active displays.

### Platform and transport constraints

- Windows ships **physical output mutation first**. A first-party IddCx virtual
  display backend remains the reviewed long-term architecture after the physical
  path proves the shared contract.
- The protocol and admission model must support benchmarking both reliable QUIC
  carrier shapes before any production default is frozen:
  1. all monitor video multiplexed over the current reliable stream; and
  2. one reliable server-to-client video stream per monitor, retaining the
     current control/audio path.
- NVENC capacity is not modeled as a hard-coded numeric cap. Admission must
  detect capacity by attempting to open and initialize the **planned session
  set** and then binding/observing the aggregate plan.

### Coordinate-space contract

- Requested topology uses the client's **logical desktop space** for placement:
  `x/y` and the explicit logical width/height describe arrangement bounds.
- Match My Layout's default stream extent is the display's **presentable
  logical workspace** expressed in pixels. A 6016x3384 external panel running
  macOS at 2x therefore requests a 3008x1692 host surface at 100% Windows UI
  scale; a 2560x1440 1x display requests 2560x1440 at 100%. This reproduces
  usable workspace rather than scaling the same HiDPI choice twice.
- HiDPI streaming is an explicit, per-display opt-in. It requests backing
  pixels instead (6016x3384 in the example) and pairs them with the measured
  2x/200% host UI scale so logical workspace remains unchanged while sharpness
  increases. The ratio is measured from logical and backing geometry; it is
  never inferred from Apple, Philips, Dell, or any other manufacturer name.
- Requested stream extent (`width_px/height_px`) is always kept separate from
  logical arrangement bounds, whether it currently carries the point-sized
  default or the backing-pixel HiDPI opt-in.
- Applied topology uses explicit **host pixel rectangles** and host-pixel
  desktop bounds. Shared helpers must never derive an aggregate pixel desktop
  extent by combining requested logical origins with physical per-monitor
  stream sizes.
- This prohibition is enforced mechanically, not by review convention. Shared
  bounds computation (`arcen_media::checked_layout_bounds`) consumes
  `SpacedLayoutRect` values that tag their origin and extent `LayoutSpace`
  (`LogicalArrangement` / `HostPixel`) and rejects a rect whose origin and
  extent disagree with `TopologyPlacementError::MixedUnitRect`.
- Rotation handling and desktop-origin handling are **explicit inputs**, never
  inferred from the platform. `TransformConvention::NativeNeedsTransform`
  declares native pre-rotation extents that shared code must rotate into an
  on-desktop footprint; `AlreadyCompositorOriented` declares an extent that has
  already absorbed the transform and must not be rotated again.
  `OriginPolicy::PreserveSigned` keeps signed desktop coordinates, while
  `TranslateToNonNegative` performs a checked shift of the whole plan.

### Wire-compatibility contract

- The host must advertise an explicit pre-auth **`AuthRequest.multi_monitor_v1`
  offer** before a client may send `AuthResponse.multi_monitor_v1`. Missing
  offer means "legacy/unsupported", so Deck must stay primary-only rather than
  sending Match My Layout v1 and relying on silent degradation.
- The existing top-level **`server_hello.monitors` field keeps its legacy
  schema exactly**. The richer applied multi-monitor roster lives only in the
  additive `multi_monitor_v1` sidecar.
- The shared **`client_display_id` is an opaque bounded non-empty string**.
  Legacy numeric `ClientMonitor.id` compatibility remains separate and must
  never be reconstructed through lossy numeric translation from that opaque id.

### Performance gate

- The release hardware targets remain **4×1080p60** and **2×4K60**.
- A **4×4K** layout is not a product promise. It is admitted only on a host
  that proves the exact plan live.
- This ADR intentionally records **no unverified numeric thresholds**. Before a
  carrier default is chosen or a product advertises multi-monitor support,
  Shared/Architecture must record the current single-monitor baseline and the
  chosen pass/fail thresholds for:
  - glass-to-glass latency relative to baseline,
  - delivered fps and queue age,
  - drop/supersession rate,
  - keyframe recovery time,
  - total/per-monitor wire bytes, and
  - fairness when one monitor is full motion and others are sparse UI.

## Consequences

- Shared crates gain validated requested/applied topology, session-monitor, and
  aggregate media-plan contracts plus the pre-auth offer gate, without changing
  current product behaviour.
- Protocol v3 remains backward compatible through additive optional fields and
  capability maps. Old peers remain primary-only because they omit the pre-auth
  multi-monitor offer, so newer Deck builds must keep auth requests
  primary-only when talking to them.
- Monitor-scoped absolute pointer and pen wire messages are deferred in this
  tranche. The shared topology generation/session-monitor foundation lands now;
  the input wire hook waits for a reviewed product integration pass.
- Session-global audio remains session-global. Multi-monitor does not add
  monitor identity to audio routing.

## Legal and provenance boundary

- The implementation and later product work must not access, copy, port, or
  derive from a local reference corpus.
- Any future source reuse remains governed by
  `legal/ORIGINS.md` and must be recorded in
  `legal/ORIGINS.md`.
- No third-party virtual display driver, SDK payload, or proprietary reference
  corpus becomes a source dependency through this ADR.

## Amendment (2026-08-10): Aggregate media-plan contract vs. measured encoder admission

### Context

This ADR's original tranche (`d484da9`) landed two things in the same commit:
`shared/media/src/multi_monitor.rs`'s `PerMonitorMediaPlan`,
`AggregateMediaBudget`, and `AggregateMediaPlan` — a declared/summed budget
model where a caller supplies `cpu_millis_per_second` and `vram_bytes` and the
type checked-sums `hardware_sessions`, `software_sessions`,
`encoder_contexts`, `pixel_rate`, and `connection_bitrate_kbps` from a list of
per-monitor plans — alongside `RegionMediaPlan`/`RegionMediaRoster`, the
host-authoritative per-region plan/roster this ADR also froze. The Decision
section above already anticipated that capacity could not be a hard-coded
numeric cap and would instead need to be detected "by attempting to open and
initialize the planned session set and then binding/observing the aggregate
plan," but at that point no measurement mechanism existed yet for either
model.

Since then, `shared/media/src/encoder_admission.rs` implemented that two-phase
measured admission gate: `RegionActivityProfile(s)` (required per-region
service rate and priority), `EncoderSetCandidate` (an exact
`RegionMediaRoster` paired 1:1 with opaque `EncoderBindingId` platform
bindings), an injectable `EncoderMeasurementAdapter`, and
`admit_encoder_sets`, which concurrently measures every candidate's real
p50/p95 encode latency, p50/p95 queue age, delivered fps, and Jain fairness
against host-supplied `EncoderAdmissionThresholds` derived from `arcen-telemetry`
QoS targets, and returns one atomic `EncoderSetDecision::{Accept, Reassign,
Reject}`. This is wired into both hosts' pre-stream runtime paths
(`hosts/linux/src/media/encoder_admission.rs`,
`hosts/windows/src/encoder_admission.rs`, both consuming
`hosts/capenc/src/admission_probe.rs`) and is the mechanism the
`encoder-set-admission`, `encoder-linux-runtime`, `encoder-windows-runtime`,
and `encoder-runtime-validation` work implemented and validated.

Auditing the current tree confirms `AggregateMediaPlan`, `AggregateMediaBudget`,
and `PerMonitorMediaPlan` are constructed and consumed only inside
`shared/media/src/multi_monitor.rs` itself (their own constructors and their
`#[cfg(test)]` module). No host, client, gateway, or `arcen-protocol` wire type
anywhere in the workspace names any of the three, and none of them was ever
placed on the wire. The measured `encoder_admission.rs` path is built entirely
on `RegionMediaPlan`/`RegionMediaRoster`, not on `PerMonitorMediaPlan`; no code
converts between the two families in either direction.

### Decision

`AggregateMediaPlan`, `AggregateMediaBudget`, and `PerMonitorMediaPlan` do
**not** compose with measured encoder admission as two cooperating stages (for
example, a cheap declared pre-check feeding a measured verification pass).
They are **superseded**, specifically and only for encoder-capacity/admission
gating, by the measured contract in `shared/media/src/encoder_admission.rs`.
Reasons:

1. A caller-supplied, checked-summed `cpu_millis_per_second`/`vram_bytes`
   budget is exactly the "hard-coded numeric cap" style of capacity modeling
   this ADR's Performance gate already rejected for NVENC. Capacity is
   authoritative only once it is measured against the real bound outputs and
   encoders, which is what `admit_encoder_sets` does and
   `AggregateMediaBudget::from_monitor_plans` does not.
2. `PerMonitorMediaPlan` independently duplicates `RegionMediaPlan`'s role
   (backend, video configuration, width/height/fps) rather than being derived
   from it, and the two have diverged: `PerMonitorMediaPlan` carries
   `bitrate_kbps`/`cursor_mode`/`degraded` but no `stream_epoch`;
   `RegionMediaPlan` carries `stream_epoch` but none of those three. Keeping
   both as the live per-monitor plan carrier would create two sources of
   truth for the same negotiated monitor.
3. Zero product, protocol, or cross-crate code constructs or consumes any of
   the three types today; they are reachable only from their own definitions
   and tests. There is no existing composition to preserve.
4. The already-approved forward roadmap (`add-region-bitrate-budget`) adds an
   explicit validated bitrate budget directly onto `RegionMediaPlan` and its
   wire `AppliedMonitorMediaPlanMsg` mapping, explicitly to delete duplicated
   nominal-bitrate helpers — confirming `RegionMediaPlan` plus measured
   admission, not `AggregateMediaBudget`, is the single forward path for both
   per-monitor plan and budget/bitrate truth. That work has since landed:
   `RegionMediaPlan` carries a required `BitrateBudgetKbps`, both hosts
   populate it from encoder planning/admission and publish it verbatim as the
   applied wire `bitrate_kbps`, and the two duplicated host
   `nominal_bitrate_kbps` helpers are gone in favor of
   `BitrateBudgetKbps::nominal_for_geometry`. `PerMonitorMediaPlan`'s own
   `bitrate_kbps`/`from_resolved` remain untouched and frozen under this
   amendment's policy below.

### Compatibility, migration, and deprecation policy

- **No public Rust API changes as part of this decision.** This amendment is
  documentation-only. `AggregateMediaPlan`, `AggregateMediaBudget`, and
  `PerMonitorMediaPlan` remain defined, exported from `arcen-media`, compiled,
  and covered by their existing unit tests exactly as they are today.
- **Frozen, deprecated-by-decision, pending removal.** Shared/Architecture
  records these three types as closed to new callers effective this
  amendment. Any new capacity/admission work must use
  `RegionActivityProfile(s)`, `EncoderSetCandidate`,
  `EncoderMeasurementAdapter`, `admit_encoder_sets`, and
  `EncoderSetDecision` from `shared/media/src/encoder_admission.rs` instead.
- **Removal is out of scope here** and is deferred to a dedicated,
  separately reviewed Shared/Architecture follow-up change that must, at a
  minimum: (a) re-confirm zero product/protocol consumption at removal time;
  (b) delete the three types and their dedicated tests in
  `shared/media/src/multi_monitor.rs`; (c) update `shared/media/ARCHITECTURE.md`
  and this ADR's status accordingly; and (d) get Shared/Architecture sign-off
  as a public shared-crate API change per `shared/AGENTS.md`, notifying any
  product owner found to depend on them by that time.
- **No wire/protocol migration is required.** None of the three types was
  ever serialized on the wire; `arcen-protocol` has no reference to any of
  them, so this decision has no wire-compatibility surface to manage.
- Until removal, `arcen-media`'s public surface keeps both families side by
  side without implying they cooperate. `RegionMediaPlan`/`RegionMediaRoster`
  together with `shared/media/src/encoder_admission.rs` are the sole
  authoritative per-monitor plan and admission contract going forward.
  `AggregateMediaPlan`/`AggregateMediaBudget`/`PerMonitorMediaPlan` are
  retained only for source/binary compatibility of their existing tests and
  any unforeseen external caller, with no capacity or admission guarantee
  attached to them from this point on.

### Consequences

- Removes ambiguity for future shared-media work: measured
  `encoder_admission.rs` is the one capacity/admission contract; the aggregate
  budget types are not an alternate or complementary path.
- `shared/media/ARCHITECTURE.md` gets a short pointer to this amendment so its
  crate-summary mention of "aggregate-plan contracts" is not read as still
  describing the live admission mechanism.
- No test, build, lint, or product behavior changes; `cargo test`/`cargo
  clippy` results for `arcen-media` and its consumers are unaffected.

## Out of scope for this tranche

- Deck or host product behaviour changes.
- QUIC multi-stream implementation or carrier selection.
- Live topology mutation during an attached session.
- Marking DISPLAY-380 complete or advertising multi-monitor as available.
