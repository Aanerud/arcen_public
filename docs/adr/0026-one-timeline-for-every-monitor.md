# ADR 0026: One timeline for every monitor's stream

**Status:** Accepted 2026-11-17. Product-wide (every Pier, both Decks).
Builds on [ADR 0014](0014-quic-lanes-and-capture-contracts.md) (one video
lane, per-monitor frame headers) and amends the multi-monitor part of
[ADR 0025](0025-arcen-joins-the-users-session.md) §6.

## Context

A Deck that matches the user's layout gets one stream per monitor: each
monitor is captured, encoded, carried and decoded on its own. The
streams arrive with different delays — a larger monitor encodes longer,
a burst on one monitor queues behind its own frames, each decoder has its
own latency — so a window dragged across two monitors used to reach the
Deck's two displays several milliseconds apart.

The earlier answer on a host with a limited encoder was to squeeze every
monitor into one canvas: one encoder, one decoder, one timeline. That
keeps the monitors in step for free, but it costs pixels (an L-shaped
layout's bounding box), refuses rotated monitors, makes every monitor
pay for the largest one's latency, and cannot follow the layout of a
desk with mixed sizes and offsets. The reference products we studied
encode per head, too.

## Decision

### 1. One Pier timeline

Every Pier stamps every access unit of every monitor on the same clock:
the wire clock (`VideoHeader.timestamp_ms`, epoch milliseconds truncated
to 32 bits, `arcen_protocol::wire::now_wire_timestamp_ms`). Nothing per
monitor, nothing per encoder: one clock per Pier.

### 2. Clock offset: NTP style, over the health ping/pong

The Deck already sends `health_ping` with its own wire time and the Pier
answers `health_pong` with the echo and its own `server_timestamp_ms`.
Those are NTP's `t1`, `t2 = t3` and (with the measured round trip) `t4`.
`arcen_media::timeline::PierClockSync` keeps the last eight samples and
takes the offset from the one with the shortest round trip (NTP's clock
filter). No wire change; every Pier already answers.

The offset is not needed to synchronise — the two clocks cancel out of the
difference between two streams' transit times — but it makes the reported
latency absolute (capture on the Pier to presentation on the Deck).

### 3. Presentation: hold the faster streams, bounded

`arcen_media::timeline::FrameSetTimeline` sees every decoded frame of a
multi-monitor session (deck-core's media worker, both Decks):

- *transit* = Deck time when decoded − Pier capture stamp, per frame;
- each stream's median transit over the last second;
- the session's playout delay = the slowest *active* stream's median (a
  stream is active when it decoded a frame in the last 500 ms; an idle
  monitor sends nothing while nothing changes on it);
- a frame waits `delay − its own transit`, at most **50 ms**;
- a lone active stream is never held, so single-monitor sessions and a
  Deck where only one monitor is changing pay nothing.

A frame that needed more than the bound is presented late rather than
stalling the others (`clamped` in the report). Frames of one monitor leave
in order; at most four wait per monitor.

### 4. Evidence

Every five seconds the media worker logs `multi-monitor timeline` at
INFO: per stream the presented and decoded transit (p50/p90), held and
clamped counts; the session's skew as presented (`skew_p50_ms`,
`skew_p90_ms`), the skew it would have had without the timeline
(`unsynced_skew_p50_ms`), the absolute capture-to-present latency and the
clock offset/round trip. `ARCEN_DECK_TIMELINE=off` measures without
holding (A/B).

The presenters add their own display's refresh phase (each window
presents at its display's next refresh), at most one refresh period; the
timeline lines up the hand-over to the presenters.

## Consequences

- Each monitor is its own stream with its own size, rotation and
  refresh; the timeline replaces the one canvas as the way the monitors
  stay in step.
- A Pier that cannot sustain one encoder per monitor degrades visibly
  (lower rate per stream, or fewer streams with a typed reason), never by
  merging streams silently.
- The Deck's multi-monitor latency is the slowest stream's, plus at most
  nothing for that stream; a faster stream gives up its head start.

## Alternatives considered

- *One canvas* (the earlier squeeze): rejected above.
- *Present on arrival* (what the Deck did): skew equals the difference in
  per-stream delay; measured in the report as `unsynced_skew_p50_ms`.
- *A per-frame-set barrier* (wait until every monitor delivered the frame
  captured at *t*): monitors are captured independently (damage-driven,
  each at its own phase), so there is no frame set to wait for; a barrier
  would stall on an idle monitor. Rejected for the transit-based playout
  delay.
- *A new clock message*: the health ping/pong already carries what NTP
  needs.
