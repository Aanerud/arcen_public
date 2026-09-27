# macOS multi-monitor: implemented, not yet qualified

Concurrent capture of several displays exists in
`hosts/macos/src/multi_capture.rs`, and the wire path now exists in
`hosts/macos/src/multi_monitor.rs` and `hosts/macos/src/stream.rs`.

That is not the same as a qualified feature. The lab Mac currently has one
display attached. The multi-display capture test therefore skips itself, and no
run tonight proved that a Deck can draw two macOS Pier monitors. Until a
two-display Mac proves capture, negotiation, region frames, and Deck
presentation together, this remains implemented-but-unqualified.

## The exchange

```
host   -> client   auth_request.multi_monitor_v1  = AuthMultiMonitorOfferMsg
client -> host     auth_response.multi_monitor_v1 = AuthMultiMonitorRequestMsg
host   -> client   server_hello.multi_monitor_v1  = ServerMultiMonitorMsg
host   -> client   region video frames
```

The applied topology must reach the client **before the first region frame**.
The macOS host now follows that order: it attaches the applied
`ServerMultiMonitorMsg` to `server_hello` and only then streams region-video
frames.

## The offer

`AuthMultiMonitorOfferMsg::new(max_monitors, supported_rotations, carriers)`
validates that the maximum is nonzero and within the protocol limit, and that
the rotation and carrier lists are non-empty and duplicate-free.

Only two carriers exist:

| Carrier | Meaning |
| --- | --- |
| `MuxedReliableStream` | every region shares the reliable media carrier |
| `PerMonitorReliableStream` | one reliable stream per monitor |

macOS advertises `MuxedReliableStream` alone. It needs no new QUIC streams, and
it is what Linux offers.

A minimal valid offer:

```rust
AuthMultiMonitorOfferMsg::new(
    2,
    vec![RotationMsg::Degrees0],
    vec![MultiMonitorCarrierMsg::MuxedReliableStream],
)
```

## Identity: the trap worth naming

`SessionMonitorId` is **nonzero**, because `monitor_id = 0` on the wire means
*legacy single-monitor frame*. `MonitorCapture::monitor_index` is zero-based
and is an internal ordering aid only. Using it as the wire id would silently
turn the first monitor's frames into legacy frames, which a client would
accept and draw in the wrong place.

## What the host produces

One `RegionMediaPlan` per negotiated monitor, collected into a
`RegionMediaRoster` (1..=4 plans, no duplicate ids). Each plan carries the
session monitor id, a `MediaStreamEpoch`, the codec actually selected, the
exact capture geometry, the frame rate and a validated bitrate budget.

`MediaStreamEpoch` fences decoder state across an encoder restart. Linux
derives it from the topology generation, which is the simplest thing that is
correct.

The applied topology is validated by `shared/media/src/applied_topology.rs`,
which requires a nonzero generation, unique nonzero monitor ids, valid
rectangles, a valid desktop bounding rectangle, and **exactly one media plan
per applied monitor** with ids matching the descriptors.

## Region frames

Frame type is chosen by whether the monitor id is nonzero. For HEVC that is
`FrameType::RegionVideoH265`, and the header carries `monitor_id`,
`topology_generation` and `stream_epoch` — all three already exist in
`VideoHeader`, so no protocol change is needed.

## Admission gates

The offer is still withheld unless all of these are true:

1. `platform.multi_monitor.advertise_enabled` is set.
2. More than one display is attached.
3. This build's multi-display capture gate is enabled.

The request is admitted against the exact offer sent on this connection. The
host checks monitor count, rotations, requested topology invariants, and carrier
intersection, assigns nonzero `SessionMonitorId`s primary-first, builds one
media plan per monitor, and reports `TopologyBackendKindMsg::PhysicalOutputs`.

## Testing

Multi-monitor cannot be qualified on a single-display machine. The test suite
must be run on a Mac with at least two attached displays, and a real Deck must
show both regions. Passing on the one-display lab Mac is only evidence that the
single-display path still works and that the multi-monitor tests skipped for
the right reason.
