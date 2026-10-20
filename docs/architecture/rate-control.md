# Live rate control

`arcen_media::rate_control::RateController` is the one controller every Pier
runs for live video. Once a second the host gives it a `RateSample`: bytes and
frames its writer delivered, mean queue wait, and the QUIC `PathSignal` (RTT
over the baseline, congestion events, lost and sent packets). It returns a new
bitrate target when the change is worth reconfiguring for.

## What counts as congestion

- **Queue delay.** RTT above the baseline by more than `congested_queue_delay`
  (15 ms) for `queue_delay_samples` (2) intervals, or three times that in one
  interval. An interval that carried a keyframe (`RateHints::keyframe_bytes`)
  is not a severe cut on its own. A keyframe is several times the size of its
  neighbours and its queue drains by itself, so the queue must still be there
  an interval later. Hosts that do not report keyframe bytes keep the old,
  immediate behaviour.
- **Loss.** A loss rate counts only when it is measured over at least
  `min_loss_sample_packets` (200) sent packets. A quiet stream pools its
  packets across intervals until it reaches that. After
  `max_loss_sample_intervals` (10) intervals the pool is judged if it holds at
  least a quarter of the minimum, and otherwise discarded as no evidence.
  - Before pooling, one lost packet in a quiet second of a 0.3 Mbps stream read
    as 2.5 % or more and cut at once.
  - An interval whose pool already holds lost packets but has not been judged
    is neither clear nor congested. It does not probe up, count towards the
    clear streak, or reset the knee. Otherwise a quiet stream under sustained
    real loss would climb back between judgments and cycle between the floor
    and the probe.
  - Loss at or above `congested_loss_rate` (2 %) is congestion.
  - Event loss (≥ 0.5 % with QUIC congestion events) is congestion when it
    persists for two evidenced windows, or when it comes with a growing queue.
- **Local writer wait.** The same streak rule, also keyframe-aware.

A cut takes the target to at most `decrease_factor` (0.72) of the previous
target, never more than half in one interval, and never below
`floor_bps` (start / 4, at least 0.5 Mbps). Clear intervals probe back up. The
probe rule and evidence gate come from the pipeline contract.

## Frame rate of Detail streams

Auto and Grading are `MotionPriority::Detail`: under a lower bitrate they keep
each frame's detail and give up frames. The host takes the rate from
`RateController::detail_framerate(contract_fps)`, which applies three rules in
order:

1. **Bits before frames.** The contract's rate holds while the target is at
   least `DETAIL_BITS_BEFORE_FRAMES` (half) of the start bitrate. Below that,
   frames are shed in proportion, never below `DETAIL_MIN_FPS` (8) or the
   contract's own rate if that is lower.
2. **Only a spending stream sheds frames.** Shedding needs the busiest pipeline
   to deliver at least 90 % of the target. A stream below 60 % of its target is
   content-limited again: a pen stroke or a typing session is far below any
   target, and dropping its frames would cost motion and save nothing.
3. **Every decision carries a reason:** `contract`, `app_limited`, or
   `bits_per_frame`. Every Pier (Linux, Windows, macOS) logs
   `encoder frame rate follows bitrate` at INFO whenever the decision changes,
   with `fps_reason` and `bitrate_bound`. The Linux Pier adds `fps_reason`,
   `bitrate_bound`, `keyframe_bytes` and the loss evidence (rate and sample
   packets) to `encoder bitrate follows QUIC path`; the Windows rate log adds
   the same apart from `keyframe_bytes`.

Speed is `MotionPriority::Motion` and never sheds frames; it gives up detail
instead.

### Why

On the Xorg lab host, a Grading stream over a home-Wi-Fi VPN sent about 40
packets a second. Per-second loss read 27–471 ‰, and IDR bursts showed
113–303 ms of queue delay. Together they held capenc at 8–14 fps for most of
a session while the screen and the encoder could do 30. Capenc's own
`fps_limiter` verdict on each stats line says which limit applied
(`hosts/linux/ARCHITECTURE.md`).

## Hosts

| Host | Detail frame rate | Keyframe hint |
| --- | --- | --- |
| Linux Pier | `RateController::detail_framerate` | yes, from the send queue's `keyframe_bytes_sent` |
| Windows Pier | `RateController::detail_framerate` | not yet |
| macOS Pier (development) | `RateController::detail_framerate`, published by the single-display rate task to the encode loop; multi-display region encoders run at the capture rate | not yet |
