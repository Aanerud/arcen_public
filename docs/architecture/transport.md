# Transport: Direct QUIC

> **STATUS (2026-07-31):** Shipped Linux and Windows Pier plus macOS Deck use
> direct QUIC exclusively, normally on UDP 18444. WSS network code is dormant
> source behind the default-off `wss-compat` feature and is excluded from
> product builds. `arcen-transport` remains dependency-light by default;
> product crates opt into its Quinn feature. Arcen Span remains dormant.

**Owner:** Shared/Architecture. See
[`../adr/0007-quic-only-product-transport.md`](../adr/0007-quic-only-product-transport.md)
for the product decision and
[`../adr/0002-transport-evolution.md`](../adr/0002-transport-evolution.md) for
the earlier migration design and exact library/version selection.

The default `arcen-transport` build supplies the shared rustls posture,
certificate validation, typed certificate/SPKI pins, atomic reload resolver,
and product-neutral contract: payload caps, reliability classes, delivery
mechanisms, bounded queues, and lifecycle events. Its default-off
`wss-compat` surface preserves dormant migration policy without placing a WSS
network path in product binaries. The opt-in `arcen_transport::quic` module
supplies both the active direct carrier and a richer stream/datagram adapter
that direct sessions do not yet use.

The shared direct-transport certificate behavior is documented in
[`tls-certificate-lifecycle.md`](tls-certificate-lifecycle.md).
Replacement QUIC connections retain this same rustls/SPKI boundary; their
separate session, grant-rotation, and resource-lifecycle contract is documented
in [`session-auto-reconnect.md`](session-auto-reconnect.md).

## Direct product QUIC profile

- Pier and Deck negotiate TLS 1.3 with ALPN `arcen-quic-v1` over Quinn.
- Both Piers use the shared reloadable certificate resolver. Deck reuses
  system/private-CA validation, certificate/SPKI pins, session-only TOFU, and
  the double-gated insecure development verifier.
- `DirectQuicStream` owns the endpoint and connection and exposes one
  full-duplex QUIC stream as Tokio `AsyncRead`/`AsyncWrite`. When its framed
  owner drops, it retains those QUIC lifetime handles for a bounded one-second
  drain so terminal control data is not overtaken by implicit connection close.
  A process-wide cap bounds concurrent drain tasks under connection churn.
- Deck writes a fixed 16-byte transport preface immediately after opening the
  stream. Pier consumes and validates it before WebSocket framing, which makes
  the otherwise server-first authentication protocol visible to Quinn without
  adding an application authorization claim.
- Deck and Pier run the existing bounded WebSocket framing, authentication,
  session, media, input, audio, clipboard, licensing, and resume logic over
  that stream. A successful QUIC/TLS handshake is never application
  authorization.
- QUIC is the only product transport. There is no selector or silent fallback.
  A host reports the already accepted socket in
  `ServerHello.negotiated_transport`; the client then confirms that same
  transport in `ClientHello.transport_capabilities`.
- Deck records a five-second QUIC feedback sample containing RTT, congestion
  window/events, loss counters, sent counters, MTU, and black-hole detections.
- The compatibility profile pins QUIC packets to the RFC 9000 1200-byte
  minimum and disables upward MTU discovery. This avoids local oversize-probe
  failures on the fleet's 1280-byte IPsec path; a future path-aware policy may
  safely raise the ceiling.

This baseline intentionally retains WebSocket framing and one reliable ordered
stream. It therefore does not yet claim independent loss domains for media and
control, datagram latency gains, or removal of TCP-like stream head-of-line
blocking within the application flow.

## Advanced QUIC adapter (not used by direct sessions)

The richer adapter remains feature-gated and tested. Default dependency-tree
CI proves Quinn/Tokio are absent from the normal `arcen-transport` graph.

1. Build your own `rustls::ServerConfig` / `rustls::ClientConfig` — your
   certificates, private keys, ALPN protocols, and certificate verifier. This
   crate never supplies a default verifier and never ships a "skip
   verification" helper.
2. Wrap that into `quinn::ServerConfig` / `quinn::ClientConfig` via
   `quinn::crypto::rustls::QuicServerConfig`/`QuicClientConfig`, and attach
   `quic::recommended_transport_config_arc(&policy)` (or your own
   `quinn::TransportConfig` with at least one concurrent bidirectional stream
   and one concurrent unidirectional stream per direction).
3. Implement `quic::PeerIdentityAuthorizer` — a required hook that inspects
   the peer's TLS certificate chain plus its claimed `QuicRole`/session and
   grant-bound expected identity, then returns an explicit accept/reject. There
   is no permissive default.
4. Construct a non-cloneable `quic::QuicAdmission` with a freshly consumed
   grant, local/expected remote identities, supported and required
   capabilities, explicit authorization, and admission time. Pass it to
   `quic::connect(QuicDialParams { .. })` (initiator) or
   `quic::accept(connection, admission, runtime)` (acceptor, on an
   already-accepted `quinn::Connection`). Both perform handshake v2 role,
   session, identity, and capability checks before returning a `QuicPeer`.
5. Drive the peer with `QuicPeer::send`, `recv_message`, `recv_event`,
   `feedback_snapshot`, and `close` — or through the object-safe
   `quic::AsyncTransportPeer` trait (boxed `Send` futures, no `async_trait`
   dependency) if your component wants to hold a `Box<dyn AsyncTransportPeer>`
   without depending on this crate's concrete type.
6. To reconnect after total connection loss, call `quic::reconnect(&old_peer,
   QuicDialParams { .. })` with a new `QuicAdmission`; it marks the old peer
   `Reconnecting`, dials with fresh replay-consumed grant evidence, and marks
   the new peer `Reconnected(new_id)` so both connection IDs are always
   available together.

Delivery is restricted by a fixed mapping: `Control` and `MediaReliable`
always use the reliable stream; `MediaLowLatency` may use the reliable stream
or the encrypted datagram path; `InputLowLatency` stays on the reliable
stream unless a future reviewed profile changes that. Encrypted datagrams
are further capped by both a conservative configurable payload limit and the
connection's live `max_datagram_size`.

`recv_message` returns an `InboundEnvelope`, preserving semantic class,
reliability, delivery, declared size, sequence, session, and authenticated peer
evidence. Fixed headers are validated against the peer-owned `SessionBinding`
and negotiated capabilities before exact payload allocation. Reliable `send`
resolves only after the frame has been accepted by Quinn's stream writer; this
is local transport acceptance, not an application-level acknowledgement from
the remote peer. Runtime setup has a bounded establishment deadline, while
inbound and outbound queues are bounded by both message count and bytes.

## Direct-monitor stream foundation (Carrier B, not product-selected)

The multi-monitor plan's Carrier A/B comparison (see the session plan's
"Carrier abstraction and QUIC A/B" section) requires a safe, additive
foundation for a per-monitor reliable stream shape before either carrier can
be measured or selected. `arcen_transport::quic` implements that foundation
today. It is **not** wired into any product path, does not change the direct
carrier's default behavior, and does not enable datagram media:

- `MonitorStreamIdentity` is a validated, bounded preface/identity: a
  control-character-free, length-bounded session id; nonzero
  `NonZeroU64` attachment and topology generations; a nonzero `NonZeroU16`
  session-monitor id; and a fixed 32-byte caller-computed media-plan
  fingerprint. `encode`/`decode` use a fixed magic and version and a bounded
  wire format, and fail closed (a typed `MonitorStreamPrefaceError`) on
  malformed, truncated, or oversized input — including an oversized claimed
  session-id length, which is rejected immediately after the fixed header is
  read, before any unbounded allocation or further read.
- `open_monitor_stream` opens one unidirectional stream on an existing
  `quinn::Connection` and writes the full encoded preface before returning
  the send stream, so the preface is already visible to the peer as soon as
  the stream is opened — proven by a loopback test in which the client
  accepts and fully parses the identity while the server's `SendStream`
  handle is still open and unfinished, with no payload written yet (no
  separate handshake round trip, and visibility does not depend on any
  later payload or `finish()`). `accept_monitor_stream`
  accepts one incoming unidirectional stream within a caller-supplied bounded
  timeout, parses and validates its preface, and returns an owned receive
  stream plus the parsed identity — or a typed error
  (`QuicTransportError::MonitorPreface`, `MonitorStreamTimedOut`, or the
  existing connection/stream error variants) on any failure.
- `MonitorStreamRoster` is a pure, synchronous, no-I/O registry for the
  expected 1..=4 monitor roster of one session/attachment/topology
  generation: it accepts each expected monitor's stream exactly once,
  rejects a stale generation, unknown session, unknown or duplicate monitor,
  or a fingerprint mismatch (without consuming that monitor's one-time slot,
  so a legitimate retry can still succeed), and reports overall readiness and
  any still-missing monitors.
- `recommended_transport_config`'s live concurrent unidirectional stream
  limit is unchanged at 1: the existing direct bidirectional stream and the
  advanced `QuicPeer` adapter's single persistent unidirectional stream keep
  the exact behavior they had before this foundation existed, on every
  product and default-feature path. A separate, additively-named
  `monitor_carrier_transport_config`/`monitor_carrier_transport_config_arc`
  pair raises only that one limit to `MAX_MONITOR_STREAMS_PER_CONNECTION`
  (4) — enough for the existing direct bidi stream plus up to four
  server-to-Deck monitor uni streams concurrently on one connection. Nothing
  in `arcen-transport` or any product crate calls these two functions; only
  `tests/quic_monitor_stream.rs`'s loopback tests that genuinely open more
  than one concurrent monitor stream use them, so `QuicPeer` and every other
  live caller stay on the unmodified legacy config with no behavior change
  until a later, separate product-wiring decision opts a real connection
  into the higher limit. `DirectQuicStream::connection_handle` exposes a
  cloned handle to the same live connection so a future product adapter can
  layer monitor streams onto a connection the direct carrier already owns,
  without changing `DirectQuicStream`'s own preface, framing, or
  drain/linger lifecycle.

Real Quinn loopback tests (`tests/quic_monitor_stream.rs`, gated behind
`--features quic`) exercise this foundation end to end: one and four
concurrent monitor streams, server-first preface visibility (the client
accepting and parsing the identity while the server's send stream is still
open, unfinished, and has carried no payload), out-of-order
registration, duplicate/unknown/stale-generation/wrong-fingerprint
rejection, malformed/truncated/oversized-claim prefaces, accept timeout,
connection close before any stream arrives, and the existing direct
bidirectional stream and its config continuing to behave identically
alongside four concurrent monitor streams on the same connection. Most of
these tests — including every malformed/truncated/oversized/timeout/close
case — run over the live, unmodified `recommended_transport_config` (limit
of 1), which is itself the strongest proof that the config revert above is
harmless; only the handful of tests that open more than one concurrent
monitor stream before any is accepted use the separate
`monitor_carrier_transport_config`.

No carrier selection, benchmark result, or default behavior change is implied
by any of the above. Selecting Carrier A or Carrier B as the production
default — and any product wiring of this foundation into Deck/Pier — is a
separate, later, reviewed decision per the session plan, made only after
recording comparative measurements against the frozen baseline.

## Carrier A/B benchmark diagnostic (measurement tool, not a selection)

`arcen_transport::quic::carrier_bench` (feature `quic`) and its example,
`cargo run --release --features quic --example
quic_multi_monitor_carrier_bench`, are a **diagnostic measurement tool**, not
a carrier decision and not a product code path. They exist to give
ADR 0009's performance gate real, reproducible numbers to compare against
once real hardware validation and the frozen baseline/thresholds exist — they
do not themselves supply either.

**What it measures.** One real `quinn::Connection` pair over localhost per
run, comparing:

- **Carrier A** — every monitor's frames multiplexed over the single existing
  reliable stream (`recommended_transport_config`, live uni-stream limit 1),
  scheduled by a deterministic bounded weighted round-robin (SWRR,
  nginx-style smooth weighting) across each monitor's bounded per-monitor
  queue. This models the planned bounded multiplexed scheduler, not a naive
  full drain of monitor 0 before advancing — a fixed monitor-0-first drain
  would misrepresent Carrier A by hiding the head-of-line cost the real
  scheduler is meant to bound. Both directions of this queue are entirely
  event-driven, never polling on a fixed interval: the task draining the
  scheduler onto the shared stream never busy-polls an empty scheduler —
  each producer signals a shared `tokio::sync::Notify` immediately after
  every successful enqueue and once more after its own completion, and the
  drain task suspends on that `Notify` (rather than looping on
  `tokio::task::yield_now`) whenever every monitor's queue is momentarily
  empty and at least one producer is still running, waking promptly on
  genuine new data or completion instead of spinning; symmetrically, a
  producer that observes its monitor's bounded queue is momentarily full
  no longer retries `try_enqueue` on a fixed sleep/poll interval — it waits
  on a second, separate `Notify` (`space_available`) that the drain task
  signals immediately after every successful pop, waking every producer
  that might be waiting on a freed slot (`notify_waiters()`, since more
  than one producer can be waiting on this shared `Notify` at once, unlike
  the single-consumer drain-side `Notify` above) rather than retrying
  blind. Both waits are always exercised as part of the run's own outer
  predicted-completion-deadline-bounded future graph, so a genuinely stuck
  scenario (for example a stalled drain) still surfaces the run's typed
  `CompletionTimeout` instead of an unbounded wait, without either wait
  needing its own separate deadline plumbing. Two unit tests
  (`carrier_a_drain_suspends_on_notify_instead_of_busy_polling_while_idle`
  and
  `carrier_a_enqueue_with_backpressure_waits_on_notify_not_a_fixed_poll_interval`)
  prove this directly, from the drain side and the producer side
  respectively: each runs its half of the scenario against a paused-clock
  Tokio runtime with the other half's event only firing after a simulated
  multi-hour idle gap, bounded by a generous real-world timeout — a
  regression back to busy-polling (drain side) or fixed-interval retrying
  (producer side) would keep registering its own timers/runnable work
  faster than the simulated gap can ever be reached, so the test fails
  (hangs, for a busy-spin regression with no timer at all; or simply times
  out well past its bound, for a fixed-interval-retry regression whose own
  timers eventually catch up but far too slowly) rather than completing
  quickly. A real, saturated, tiny-payload (1-byte) Quinn loopback test
  (`carrier_a_completes_a_saturated_tiny_payload_run_without_dropping_or_stalling`)
  additionally proves the producer-side wake-on-space path delivers every
  frame correctly, in order, with zero completion failures, under genuine
  backpressure over a real connection — not just in the isolated unit
  proof above.
- **Carrier B** — one reliable unidirectional stream per monitor, opened via
  the existing `open_monitor_stream`/`accept_monitor_stream` foundation over
  `monitor_carrier_transport_config` (test-only uni-stream limit 4).

Both carriers send the identical configured workload (frame count or
duration, payload size, monitor count, active pattern) once per run, so A and
B are always compared against the same input, not resampled traffic. Each
frame's envelope carries a fixed 28-byte header (magic, version, monitor id,
sequence, send timestamp, payload length) ahead of a payload deterministically
derived from `(monitor_id, sequence)` via a splitmix64 generator, so the
receiver regenerates and byte-compares the expected payload and rejects any
out-of-order sequence per monitor instead of trusting the wire.

**Metrics recorded** (per monitor and aggregate): sent/delivered frame and
byte counts (`sent_bytes`/`delivered_bytes`/`total_sent_bytes`/
`total_delivered_bytes`, and the throughput fields derived from them, count
**application payload bytes only** — `payload.len()` — never the 28-byte
wire envelope; see "Byte-based safety math uses encoded, not payload-only,
bytes" below for the distinct wire-byte figure used internally for caps and
deadlines, which is not itself an exposed metric), elapsed wall time,
throughput, first-frame latency (the
chronologically first delivered frame's own latency, captured before any
percentile sort — not the post-sort minimum), p50/p95/p99 receive latency
(nearest-rank percentiles — the smallest observed value such that at least
that percentage of frames have equal-or-lower latency), maximum
inter-arrival ("starvation") gap per monitor, and completion/recovery
failure counts (payload mismatch, ordering violation, malformed frame,
incomplete stream, and monitor-id mismatch — both carriers' receivers
always validate, in normal release code, that every delivered frame's
envelope `monitor_id` matches the accepted identity for that path: Carrier
B checks it against the accepted per-monitor stream's own parsed identity;
Carrier A's single shared reader demuxes each frame by its envelope
`monitor_id` into that monitor's own per-monitor consumer channel — see
"Carrier-neutral `receiver_delay`" below — and a frame naming a monitor id
no consumer path exists for is rejected as `UnknownMonitorId` rather than
silently dropped or misattributed; both carriers' identity/payload/ordering
checks happen inside the same shared `PerMonitorValidator::record` call, at
the same pipeline stage, for both). **Every latency figure above (first-
frame latency and every percentile) is measured from frame-send to
post-consume observation, and — for both carriers alike — that
observation point is *after* the shared per-monitor consumer stage
(`carrier_receive_consume_one`, used by both Carrier A and Carrier B) has
applied any configured `receiver_delay`, not before it**: latency therefore
includes the configured delay's own cost as part of what it reports, for
both carriers identically, over monotonic per-frame send-to-receive deltas
when the process's own clock is stable enough to be meaningful — this
remains explicitly not a network-only or glass-to-glass measurement (see
"Non-claims" above). Fairness is reported as
**two separate, distinctly named values, never conflated**: `fairness_index`
is Jain's fairness index computed over each monitor's **delivery ratio**
(delivered bytes ÷ sent bytes for that monitor in that run, normalized
against what each monitor was actually offered), so a fully successful run
reports `fairness_index` near `1.0` under either active pattern regardless
of the synthetic workload's own per-monitor volume shape; separately,
`delivered_bytes_max_min_spread_ratio` is the raw max-to-min ratio of
delivered byte *volume* across monitors, which is expected to differ by
pattern (e.g. `one-active-rest-idle` intentionally sends far more to the
active monitor) and must not be read as a fairness/unfairness signal on its
own. An optional deterministic artificial receiver delay can be injected to
make relative starvation/fairness effects visible without depending on real
host jitter — see "Carrier-neutral `receiver_delay`" below for exactly how
and where it is applied.

**Workload timing semantics.** `--frames <N>` is an exact, deterministic,
**unpaced** tick count: producers emit ticks back-to-back as fast as the
scheduler/stream allows, bounded only by the total-bytes safety cap
(`MAX_BENCH_TOTAL_BYTES`) and the run's own completion deadline below — this
mode makes no timing claim and its `elapsed` reflects genuine transfer
speed. `--duration <e.g. 10s>` is, by contrast, **wall-clock paced**: it is
first deterministically resolved to a tick count via `BENCH_TICK_INTERVAL`
(2ms), and each producer sleeps until `epoch + tick * BENCH_TICK_INTERVAL`
before considering each tick, plus one additional final wait until `epoch +
tick_count * BENCH_TICK_INTERVAL` after its last tick — so a `--duration
10s` run spans the full requested wall-clock interval end-to-end, not one
whole `BENCH_TICK_INTERVAL` short of it. That final wait matters most at the
smallest representable duration: without it, a `--duration 2ms`
(`MIN_BENCH_DURATION`) run's single tick would target `epoch + 0` — i.e.
effectively unpaced — a 100% relative undershoot that a 1-second run's
0.2%-scale version of the same gap would never surface. A `--duration 10s`
run's `elapsed` mostly reflects the requested duration itself, not delivery
throughput, regardless of how fast the underlying transfer could otherwise
complete. `--duration` is only representable at `BENCH_TICK_INTERVAL`
granularity — a value below `BENCH_TICK_INTERVAL` (2ms) cannot resolve to
even one meaningfully-paced tick and is rejected by `BenchConfig::validate`
as `DurationOutOfRange` (`MIN_BENCH_DURATION == BENCH_TICK_INTERVAL`), and a
duration that is not an exact multiple of `BENCH_TICK_INTERVAL` is truncated
down to the nearest whole tick (e.g. 5ms resolves to 2 ticks, not 2.5).
Every run — either workload shape, either carrier — is additionally bounded
by a predicted outer completion deadline: the larger of the workload's own
production-time budget and a deliberately pessimistic data-transfer budget,
plus any `receiver_delay`-driven completion floor (the busiest single
monitor's own frame count times the delay — see "Carrier-neutral
`receiver_delay`" below for why it is per-monitor, not summed across every
monitor), plus a small fixed drain allowance for stream-finish/task-join
jitter. If a run does not
finish within that deadline, it returns a typed `CompletionTimeout` error
instead of hanging indefinitely — a stall/backpressure safety net, not a
latency or throughput claim. Separately, and more importantly,
`BenchConfig::validate` rejects up front any `--duration`/
`--receiver-delay-ms` combination whose own arithmetic already guarantees an
unreasonably long completion floor (e.g. a long duration combined with the
maximum receiver delay, which by simple multiplication could otherwise imply
a run lasting hours or days) — such a config never starts a connection or a
task, it fails validation immediately.

**Structured task ownership, cancellation, and cleanup on every
`?`/timeout exit.** Every task a carrier run ever spawns — Carrier A's
per-monitor producer tasks, its single demux *reader* task, and its
per-monitor *consumer* tasks; Carrier B's per-monitor sender tasks,
per-monitor reader tasks, and per-monitor consumer tasks — is spawned
directly by `run_carrier_a`/`run_carrier_b` themselves, never by another
task the run itself spawned. `run_carrier_a`/`run_carrier_b` build every
per-monitor channel and spawn every consumer task *before* spawning the
reader task(s) that will feed those channels; a reader task
(`carrier_a_receive_all`, `carrier_b_receive_one`) only validates and
demuxes frames and forwards each to the channel its caller already handed
it — it never itself creates a channel or spawns a task. This makes every
task in a run's entire task graph a leaf with no children of its own, one
level deep from `run_carrier_a`/`run_carrier_b`, which is what makes the
cleanup guarantee below unconditional rather than best-effort: there is no
task that could itself be aborted while mid-way through cleaning up
further tasks of its own, because no task ever owns another task. (An
earlier revision of this module had each reader task create its own
per-monitor channels and spawn its own consumer tasks internally: correct
on every path the reader itself completed or returned an error through,
but if the *reader* task itself was externally aborted — which
`cleanup_failed_carrier_a_run`/`cleanup_failed_carrier_b_run` must be able
to do, since a reader can be the very task stuck on unfulfillable QUIC I/O
that is causing the failure/timeout in the first place — Tokio only runs
that reader's own synchronous `Drop` on cancellation, never a further
`.await`, so its own nested consumer handles were only reachable via their
own `Drop`, i.e. fire-and-forget `abort()`, never a confirmed join. Hoisting
every channel/consumer-task creation up into the outer run functions
removes that scenario entirely rather than narrowing it: cleanup now always
aborts+joins consumer tasks directly from its own top-level scope, never by
delegating to a reader task that might itself be the one being aborted.)

Every one of these leaf tasks is owned via
`tokio_util::task::AbortOnDropHandle` rather than a bare
`tokio::task::JoinHandle` for its entire lifetime. A bare `JoinHandle`
dropped without being awaited only detaches its task (the task keeps
running unbounded in the background); an `AbortOnDropHandle` dropped the
same way instead aborts it, and Tokio's `abort()` schedules that
cancellation directly on the runtime without depending on the target
task's own future ever receiving an external wakeup — so this also
correctly unblocks a task parked on a `tokio::sync::Notify`/bounded-channel
wait that would otherwise never fire on its own. Because Rust drops every
local (including the untouched remainder of a `Vec`/`for` loop and an
`async` block's own captured state) on every exit path — a normal return,
an early `?` return, or the future being dropped when the outer
`tokio::time::timeout` elapses — this cascades through every current and
future early-return path in this module without each one needing individual
auditing. On top of that automatic cascade, `run_carrier_a`/`run_carrier_b`
keep every one of their own producer/sender, reader, and consumer task
handles (and, for Carrier A, its shared send stream) reachable in their own
outer scope — not moved into the timeout-bounded `completion` future by
value — so that on any non-success exit they can also explicitly `abort()`
and then `.await` every one of those handles directly (confirming those
tasks have actually finished, not merely "abort requested", before
returning), explicitly `reset()` Carrier A's shared send stream, and
explicitly `close()` both connections — closing a connection immediately
resets every stream still open on it and unblocks any task still parked on
that connection's own `accept_uni`/`accept_bi`/read/write calls with a
prompt connection-level error, faster and more direct than relying on task
abortion alone to tear down an I/O-blocked task. None of this touches a
successful run's connections/streams or its result. Explicitly `abort()`-ing
and `.await`-ing every remaining handle only works if every one of them is
still actually reachable in that outer scope at the moment of failure —
`Vec::drain(..)`, `Option::take()`, and a plain `for handle in owned_vec`
loop all remove their element(s) from the collection up front, before that
element's own `.await` resolves, so if the future doing the iterating is
itself dropped mid-loop (the outer `timeout` elapsing) or exits early via a
sibling `?`, any handle already removed from the collection is gone from
the outer scope's variable and can only ever be reached via its own `Drop`
— a fire-and-forget `abort()`, never an awaited join, so nothing guarantees
the task (and its `LiveTaskGuard`) has actually finished by the time the
caller returns. `run_carrier_a`/`run_carrier_b`'s `completion` blocks
instead use small helpers (`join_last`/`join_taken`) that await the target
handle *in place*, by `&mut` reference, and remove it from the
`Vec`/`Option` only after that specific join resolves — so a handle is
only ever missing from the outer scope once it has actually been joined,
never merely because iteration passed over it. `carrier_bench_live_task_count()`
(diagnostic/test instrumentation, not a stability-guaranteed metric, not
part of any `CarrierRunResult`/`ComparisonResult` output) exposes a
process-wide count of not-yet-finished tracked tasks purely so
`tests/quic_carrier_bench_task_lifecycle.rs` can assert this directly: that
file forces, for both carriers, a genuine outer `CompletionTimeout` (via a
receiver connection deliberately mismatched to an unrelated pair, so it
blocks on real, unfulfillable QUIC I/O) and a genuine early, non-timeout
`?` error (by closing a real run's own connection partway through a paced,
multi-tick run). Every scenario runs the carrier as a background task
rather than awaiting it directly and, before letting the forced failure
actually happen, sleeps briefly and asserts every producer/sender, reader,
and consumer task is genuinely still alive at that point — proving the
eventual cleanup is torn down from tasks that had actually started and, for
the mid-run-close scenarios, were genuinely still in flight — then lets the
real timeout/close take effect and asserts, with a single plain equality
check the instant `run_carrier_a`/`run_carrier_b` returns — no polling or
bounded retry loop — that the live task count is already back to its
pre-run baseline, and that a fresh, ordinary run completes successfully
immediately afterward, proving neither a leaked task nor any other
global/connection state blocks future runs.

**Carrier-neutral `receiver_delay`.** `--receiver-delay-ms <N>` injects a
deterministic artificial delay that is, by design, a **per-monitor,
post-demux consumer/validation delay** — applied identically by both
carriers via the exact same shared function
(`carrier_receive_consume_one`), on an independent path per monitor,
running in parallel across every other monitor's own delay. Both carriers'
receive pipelines are structurally identical from immediately after
transport framing onward: each has its own thin *reader* stage that only
validates/parses enough to demux the frame to the right monitor (Carrier
A's single shared reader demuxes each decoded frame by its envelope
`monitor_id` into that monitor's own bounded channel and rejects an
unrecognized id as `UnknownMonitorId`; Carrier B's per-monitor reader
accepts its stream and parses its `MonitorStreamIdentity` once, up front),
and both hand every frame to the identical shared *consumer* stage
(`carrier_receive_consume_one`) running on an independent, parallel task
per monitor — one consumer task for Carrier A's per-monitor channel,
one for Carrier B's per-monitor stream. Neither reader stage sleeps or
captures a receive timestamp itself; both stay entirely within ordinary
bounded-channel backpressure if a monitor's consumer falls behind.

The shared consumer applies the configured `receiver_delay` (if nonzero)
*first*, and only afterwards captures `receive_elapsed` and calls
`PerMonitorValidator::record` — which performs the frame's
identity/payload/ordering validation and its latency/gap bookkeeping, all
at this one shared stage, for both carriers alike. Capturing the
timestamp *after* the delay (not before it) is what makes `receiver_delay`
carrier-neutral in the *recorded latency itself*, not merely in overall
completion-time floor: every latency metric — first-frame latency and
every percentile — reflects frame-send to post-consume observation,
*including* the configured delay's own cost, at exactly the same pipeline
point for both carriers. A prior revision of this module captured the
receive timestamp *before* applying the delay for both carriers (so
neither carrier's recorded latency reflected the delay's cost at all), and
before that, applied `receiver_delay` inline on Carrier A's single shared
reader after every frame from every monitor, serializing its total cost
across the whole run (`total_frames * receiver_delay`) while Carrier B paid
only a parallel, per-monitor cost (`max_per_monitor_frames *
receiver_delay`) for the exact same configured value — a purely
architectural asymmetry unrelated to the delay's intended meaning, which
made the two carriers' completion times (and, in the earlier revision,
their recorded latencies) incomparable under any nonzero `receiver_delay`.
Because `receiver_delay` is now genuinely per-monitor and parallel for both
carriers, every cap and deadline computation that accounts for it —
`BenchConfig::validate`'s `ReceiverDelayFloorExceedsCap` check and
`predicted_completion_deadline`'s receiver-delay term — is keyed to
`max_per_monitor_offered_frame_count(config)` (the single busiest monitor's
own frame count: under either active pattern, at least one monitor is
fully active every tick, so this is simply the resolved tick count, with no
pattern branching needed), not the total across every monitor
(`offered_frame_count`, which remains correct, and unchanged, for the
total-bytes cap — that one *is* inherently additive across monitors). This
both raises the practically usable `receiver_delay` ceiling for larger
monitor counts (it no longer scales down with `monitors`) and, more
importantly, means the same configured delay now produces a comparable
predicted completion floor, a comparable real completion time, *and* a
comparable recorded latency for both carriers — a real, nonzero-delay
Quinn loopback test
(`four_monitor_receiver_delay_yields_a_comparable_predicted_floor_for_both_carriers`)
asserts both the completion-time and the recorded-p50-latency
comparability directly, not merely the predicted floor.

**Offered load is pattern-aware, not a blanket over-count.** Every cap and
budget above that depends on "how many frames will this run actually
produce, in total, across every monitor" — chiefly the total-bytes cap and
the completion deadline's transfer budget — is computed by one single
authoritative function, `carrier_bench::offered_frame_count(config)`,
rather than each caller re-deriving its own approximation. (The
receiver-delay completion floor and cap are a related, but distinct,
*max-per-monitor* — not total — figure; see "Carrier-neutral
`receiver_delay`" above for why.) For `all-active`, `offered_frame_count` is
simply `monitors * effective_frames` (every monitor produces every tick).
For `one-active-rest-idle`, only one monitor produces every tick; the
remaining monitors each produce only once every 10th tick
(`IDLE_DUTY_CYCLE_TICKS`), so the correct total offered load is
`effective_frames + (monitors - 1) * ceil(effective_frames / 10)` —
noticeably smaller than treating every monitor as fully active would
suggest. Prior to this fix, `validate` and the completion-deadline
computation both used the flat `monitors * effective_frames` approximation
regardless of pattern, which over-counted `one-active-rest-idle`'s true
offered load roughly ten-fold for the idle monitors and could **wrongly
reject** an otherwise well within-cap configuration (over-counting is a
correctness bug here, not a "safe" conservative simplification, once caps
are meant to reflect the load a run will actually produce). A named
regression test
(`offered_frame_count_regression_600s_duration_4096_byte_payload_is_accepted_for_one_active_rest_idle`)
demonstrates a configuration the old flat formula would have wrongly
rejected (a 600s/4096-byte-payload case tripping the old total-bytes cap)
that is now correctly accepted once the pattern-aware count is used. The
comparison's JSON and human summary both report this exact figure as
`offered_frame_count`, so a caller can independently sanity-check delivered
totals against the true offered load rather than an approximation of it.

**Byte-based safety math uses encoded, not payload-only, bytes.** Every
frame this module sends puts a fixed 28-byte wire envelope
(`BENCH_FRAME_HEADER_BYTES`) on the stream ahead of its `payload_bytes`
payload — `carrier_bench::encoded_frame_bytes(payload_bytes)` (saturating
`BENCH_FRAME_HEADER_BYTES + payload_bytes`) is the single authoritative
per-frame byte figure every byte-based safety computation must use:
`BenchConfig::validate`'s `TotalBytesExceedsCap` check
(`offered_frame_count(config) * encoded_frame_bytes(payload_bytes)` against
`MAX_BENCH_TOTAL_BYTES`) and `predicted_completion_deadline`'s
data-transfer budget both multiply the offered frame count by
`encoded_frame_bytes`, not by `payload_bytes` alone. This matters most at
small configured payload sizes and large frame counts: with a 1-byte
payload, the fixed header is *28 times* the payload itself, so a
payload-bytes-only computation would badly under-estimate both the true
wire byte total (risking accepting a config that actually exceeds
`MAX_BENCH_TOTAL_BYTES` once the real envelope is counted) and the
completion deadline's transfer budget (risking a false `CompletionTimeout`
for an otherwise entirely legitimate many-tiny-frame workload). Two named
regression tests demonstrate both directions:
`encoded_frame_bytes_regression_tiny_payload_multi_million_frame_count_gets_a_sufficient_completion_deadline`
(a 1-byte-payload, `MAX_BENCH_FRAMES`-count config whose corrected deadline
is materially larger than the old payload-only formula would have
produced) and
`encoded_frame_bytes_regression_a_payload_size_the_old_formula_would_have_wrongly_accepted_is_now_rejected`
(a 520-byte-payload config the old payload-only cap check would have
wrongly accepted, that `validate` now correctly rejects once the header is
counted). This is deliberately distinct from the `sent_bytes`/
`delivered_bytes` metrics described above, which remain **payload-only** —
`encoded_frame_bytes` is purely an internal safety-math figure, not itself
an exposed metric field.

**How to run it.**

```text
cargo run --release --features quic --example quic_multi_monitor_carrier_bench -- \
    --monitors 4 --frames 2000 --payload-bytes 65536 \
    --pattern all-active --carrier both
```

`--monitors 2|4`; exactly one of `--frames <N>` (unpaced) or `--duration
<e.g. 10s>` (wall-clock paced, see above); `--payload-bytes <N>`; `--pattern
all-active|one-active-rest-idle`; optional `--receiver-delay-ms <N>`;
`--carrier a|b|both` (default `both`). Every flag above is a singleton:
supplying any of them a second time (e.g. `--monitors 2 --monitors 4`) is a
fail-closed `DuplicateArg` parse error, never a silent last-value-wins
overwrite — this includes the `--frames`/`--duration` mutual-exclusion
check, which only runs once the whole argument list has been scanned, so a
duplicated `--frames` or `--duration` is reported as `DuplicateArg`, not
`ConflictingWorkload`. This binary always writes stable JSON to stdout by
default — there is no `--json` flag, JSON emission is not optional or gated
behind any argument — and a concise human summary to stderr, so `--carrier
both > result.json` captures only the JSON.
`parse_cli_args`, the scheduler, the frame codec, the workload-pacing/
completion-deadline helpers, and the metrics math are covered by
deterministic unit tests, and 2/4-monitor loopback completion for both
carriers and both patterns — plus paced 1-second and `MIN_BENCH_DURATION`
(2ms) `--duration` runs' actual wall-clock elapsed time for both carriers,
a 4-monitor `receiver_delay` run proving both carriers' real completion
times *and* recorded p50 latencies stay comparable (never one a multiple
of the other, regressing the old carrier-A-serial/carrier-B-parallel bias,
or the older pre-delay-timestamp-capture bug where the delay's cost was
invisible in recorded latency at all), a saturated tiny-(1-byte-)payload,
high-frame-count run proving Carrier A's wake-on-space backpressure
delivers every frame correctly under genuine queue saturation rather than
dropping or stalling, and the immediate (sub-second) rejection of a
pathological long-duration/high-receiver-delay config — is covered by
real-Quinn integration tests in `tests/quic_monitor_carrier_bench.rs`; the
heavier
multi-thousand-frame diagnostic case is `#[ignore]`d so it never becomes a
flaky CI timing gate, and is run explicitly with `--ignored` instead.
Structured task-ownership/cancellation cleanup on a forced `CompletionTimeout`
or a forced early transport error, for both carriers, is covered separately
by `tests/quic_carrier_bench_task_lifecycle.rs` (see "Structured task
ownership, cancellation, and cleanup on every `?`/timeout exit" above).

**Top-level JSON shape.** `ComparisonResult::to_json()` (what the binary
writes to stdout) always has this exact stable top-level shape, so a parent
process can parse it reliably without depending on this document's prose:

```jsonc
{
  "config": {
    "monitors": 2,                     // 2 or 4
    "workload": "frames:2000",         // "frames:<u64>" or "duration_ms:<u128>"
    "payload_bytes": 65536,
    "pattern": "all-active",           // "all-active" or "one-active-rest-idle"
    "receiver_delay_nanos": 0,
    "offered_frame_count": 4000        // authoritative pattern-expanded total, see above
  },
  "carrier_a": { /* CarrierRunResult, or JSON null if --carrier excluded it */ },
  "carrier_b": { /* CarrierRunResult, or JSON null if --carrier excluded it */ }
}
```

Each present `carrier_a`/`carrier_b` value is a `CarrierRunResult`:
`"carrier"` (`"carrier_a"` or `"carrier_b"`), `"per_monitor"` (an array of
per-monitor metric objects, ordered by monitor id), and `"aggregate"` (the
cross-monitor aggregate metric object).

**Exact field names, and where each failure/mismatch counter lives.** A
parent parsing this JSON should look for these exact keys — there is no
field literally named `"mismatch"` anywhere in this document; the closest
names are `monitor_id_mismatches` (per-monitor) and
`total_monitor_id_mismatches` (aggregate), spelled out in full below.

Each `per_monitor[i]` object (`PerMonitorMetrics::to_json`) has:
`monitor_id`, `sent_frames`, `sent_bytes`, `delivered_frames`,
`delivered_bytes`, `elapsed_secs`, `throughput_bytes_per_sec`,
`first_frame_latency_nanos` (nullable), `p50_latency_nanos` (nullable),
`p95_latency_nanos` (nullable), `p99_latency_nanos` (nullable),
`max_inter_arrival_gap_nanos`, `ordering_failures`, `payload_failures`,
`monitor_id_mismatches`, `completion_failures`, `recovery_failures`
(always `0`).

The `aggregate` object (`AggregateMetrics::to_json`) has: `total_sent_frames`,
`total_sent_bytes`, `total_delivered_frames`, `total_delivered_bytes`,
`elapsed_secs`, `aggregate_throughput_bytes_per_sec`, `fairness_index`,
`delivered_bytes_max_min_spread_ratio` (a string `"inf"` sentinel, not a
JSON number, if any monitor delivered zero bytes while another delivered
more than zero — JSON has no numeric infinity literal),
`total_completion_failures`, `total_monitor_id_mismatches`,
`total_recovery_failures` (always `0`). **`aggregate` only sums three of
`per_monitor`'s six failure/mismatch counters** — `completion_failures` →
`total_completion_failures`, `monitor_id_mismatches` →
`total_monitor_id_mismatches`, and `recovery_failures` →
`total_recovery_failures`. It does **not** provide summed
`ordering_failures` or `payload_failures` fields (no
`total_ordering_failures`/`total_payload_failures` key exists) — those two
per-frame validation signals are only ever reported per monitor, inside
each `per_monitor[i]` object; a parser that wants a run-wide ordering/
payload failure count must sum `per_monitor[*].ordering_failures`/
`per_monitor[*].payload_failures` itself rather than looking for it on
`aggregate`.

See `CarrierRunResult::to_json` and `PerMonitorMetrics`/
`AggregateMetrics`'s own `to_json` in `carrier_bench.rs` for the
authoritative implementation these field lists are derived from.

**What it is not, and the gate before selection.** Matching ADR 0009's
performance gate and the non-claims below: this tool runs one process on
localhost with no real display, capture, encode, decode, or presentation in
the loop, so it is **explicitly not a glass-to-glass measurement** and its
numbers say nothing about real network conditions, real Deck/Pier CPU or GPU
load, or real monitor content. Any JSON captured from it is a non-committed
session/diagnostic artifact, not a frozen production baseline — ADR 0009
already requires Shared/Architecture to record the single-monitor baseline
and pass/fail thresholds separately, and this tool does not do that on its
own. **No production carrier default may be selected from this tool's
localhost output alone.** Carrier selection requires, at minimum, the same
comparison run end-to-end against real hardware (pier-linux.example.internal and an actual
Deck client) with real capture/encode/decode/presentation in the loop, per
ADR 0009's performance gate, before either carrier is wired into Deck/Pier or
advertised as a product default.

## Known gaps and non-claims

The following must not be inferred from the direct carrier or advanced-adapter
tests:

- **No multi-stream/datagram product optimization.** Direct sessions currently
  use one reliable bidirectional stream and retain WebSocket framing. The
  advanced adapter's independent streams and datagrams are not product
  traffic, and the direct-monitor stream foundation above is an unselected,
  unwired building block, not a shipped Carrier B.
- **No 0-RTT.** This adapter does not enable 0-RTT connection establishment.
  If a future revision adds it, that requires its own reviewed ADR and
  threat-model update (0-RTT data is replayable).
- **No adaptive FEC.** `FecPolicy` only ever reports `Unsupported` or
  `Disabled`. There is no forward-error-correction implementation in this
  crate, adaptive or otherwise.
- **No cross-platform or kernel-level impairment proof.** The network test
  suite (`tests/network/quic_impairment.rs`) is a pure, deterministic,
  in-process model of loss/reorder/duplication/lateness — not a live
  `tc`/`netem`, namespace, or hardware network test. It validates contract
  semantics, not real-world network behavior on any specific OS or NIC.
- **No broad performance claim.** Two matched fleet runs established that QUIC
  works end to end and can modestly improve latency/delivery on some hosts, but
  Deck presentation and audio bottlenecks dominate the operator experience.
  Product direction is not evidence of a universal throughput or FPS gain.
- **Migration relies entirely on Quinn.** This crate does not implement or
  control QUIC path migration; it only observes address/MTU changes that
  Quinn's own connection-migration logic already performed and reports them
  as `PathChanged`/`MtuChanged` events.
- **Advanced-adapter telemetry is not automatic.** Direct Deck sessions log
  sampled feedback, but `recv_event`/`AsyncTransportPeer::recv_event` still
  require an explicit consuming-product bridge.
- **Feedback events are sampled and bounded.** A slow event consumer can miss
  periodic feedback/path samples; `QuicPeerCounters::events_dropped` makes that
  pressure observable. Binding and closure state should also be reconciled from
  the peer API rather than treating the event stream as durable storage.
- **No compliance or certification claim.** As with the rest of Arcen's
  security posture (see
  [`../security/trust-boundaries.md`](../security/trust-boundaries.md)),
  nothing here should be read as a certification, regulatory compliance, or
  completed independent cryptographic review claim.
- **The Carrier A/B benchmark diagnostic is not a carrier decision.** It is a
  localhost, single-process measurement tool over real Quinn connections, not
  a glass-to-glass measurement and not evidence of real network, capture,
  encode, decode, or presentation behavior. See "Carrier A/B benchmark
  diagnostic" above; real hardware (pier-linux.example.internal/Deck) end-to-end validation
  against ADR 0009's recorded baseline and thresholds is required before any
  production carrier default is selected.
