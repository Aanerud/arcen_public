# ADR 0013: One Ordered Session Stream, Keyframe Recovery, No FEC

**Status:** Accepted 2026-09-27 by the project owner.

> Records why Arcen keeps every display's video on the one ordered session
> stream, recovers from loss with keyframes rather than forward error
> correction, and which cheaper recoveries may be added inside that choice.

## Context

A Match My Layout session carries the control channel and every display's
video on one reliable, ordered QUIC stream
(`MultiMonitorCarrierMsg::MuxedReliableStream`,
`shared/protocol/src/multi_monitor.rs`). Audio already has its own priority
stream (`arcen_transport::quic::PriorityAudio`). QUIC retransmits every lost
packet, so a loss costs waiting, not picture damage: everything queued behind
it, on every display, waits at least one round trip.

Established remote-desktop designs take different routes:

- A commercial remote-workstation protocol runs one session over UDP with
  selective retransmission and forward error correction, and refines images
  progressively toward lossless.
- An open-source QUIC remote desktop sends video unreliably with 30% fountain
  code repair symbols, and keeps input on its own reliable lane.
- Open-source game streaming uses Reed–Solomon FEC (about 20%) and
  reference-frame invalidation, while another open-source remote desktop uses
  a reliable ordered stream, no FEC, and asks the host for a fresh keyframe
  when its decode queue backs up.

Three options were weighed:

| | Change | Removes | Cost |
| --- | --- | --- | --- |
| A | One reliable stream per display (`PerMonitorReliableStream`, defined, unwired) | One display stalling the other | Displays no longer share one order, which puts frame-accurate audio/video sync at risk |
| B | Video as datagrams with FEC | Most retransmit waits | A standing 20–30% bandwidth tax, block latency, fragmentation to the 1,200-byte VPN MTU, and keyframe recovery is still needed for bursts |
| C | One stream per frame, stale frames dropped | Waiting for superseded frames | Frames of different displays are no longer ordered against audio |

## Decision

Keep one ordered session stream for control and all displays' video. Do not
add forward error correction. Recover from loss with keyframes.

Reasons:

1. Frame-accurate audio/video sync across displays is a product requirement,
   and one order is the simplest way to keep it.
2. FEC is paid in bandwidth all the time, loss or not, and still needs
   keyframe recovery under bursty loss; on the links Arcen targets, keyframe
   recovery is good enough.
3. QUIC already retransmits; the cost that remains is waiting, which the
   shared transport keeps short (`keep_send_window_interactive`, adaptive
   bitrate, and `ARCEN_QUIC_CONGESTION=bbr` under evaluation).

## Consequences

- A loss delays every display by at least one round trip. Latency work goes
  into keeping the send window and queues short, not into the carrier.
- Recovery may become cheaper without changing this decision: NVENC
  intra-refresh instead of full keyframes, and reference-frame invalidation
  before a full keyframe, as Moonlight does.
- `PerMonitorReliableStream` stays in the protocol enum, unused, until a
  requirement other than latency justifies revisiting sync.
