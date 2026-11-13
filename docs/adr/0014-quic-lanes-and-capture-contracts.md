# ADR 0014: QUIC Lanes, Capture-Source Evidence and Preset Admission Contracts

**Status:** Proposed 2026-10-17; formal Shared/Architecture and
Release/Security acceptance is still pending. The contracts shipped in 0.17.0:
every product (Linux, Windows and macOS Piers, the macOS Deck) runs each
session on these lanes (protocol v5, Amendment 2). Amendments 1–18 follow the
decision; each records one later contract:

| # | Date | Contract |
| --- | --- | --- |
| 1 | 2026-10-17 | Lane runtime decisions |
| 2 | 2026-10-17 | Product adoption |
| 3 | 2026-10-17 | Capture admission before `READY` |
| 4 | 2026-10-17 | Automatic provider selection and the host qualification record |
| 5 | 2026-10-17 | Verified GPU import and the paced Speed output |
| 6 | 2026-10-17 | Liveness, capture restart and Wayland output generations |
| 7 | 2026-10-18 | Orderly session close |
| 8 | 2026-10-18 | Speed evidence without operator sizes, the runtime guard, development records and automatic re-qualification |
| 9 | 2026-10-18 | Wayland software encode and hosts without NVENC |
| 10 | 2026-10-18 | Wayland Grading and HDR |
| 11 | 2026-10-18 | Host cursor images on the input/cursor lane |
| 12 | 2026-10-19 | Arrival pacing and the paced Xorg head |
| 13 | 2026-10-19 | Pen barrel-button capability |
| 14 | 2026-10-19 | Committed text on the input/cursor lane |
| 15 | 2026-10-19 | A served-pipeline remedy for the user |
| 16 | 2026-10-20 | Desktop admission requires polkit to treat the session as remote |
| 17 | 2026-11-08 | Cursor images from every Pier that can read them (0.18.0) |
| 18 | 2026-11-09 | Windows Grading keeps the desktop's sRGB code values (0.18.0) |
| 19 | 2026-11-09 | Every Pier states `srgb` for Grading; PQ-to-SDR uses the sRGB curve (0.19.0) |

> Records the shared, portable contracts the headless Linux Wayland provider
> and the isolated audio/input transport build on: QUIC lane roles and their
> preface, lane ownership and failure semantics, capture-surface evidence,
> per-preset admission for a capture source, and explicit desktop-provider
> selection. When it was proposed nothing here had changed a shipped Pier or
> Deck; 0.17.0 shipped it.

## Context

Two independent needs meet here.

1. **Isolation.** Linux Pier services priority audio and video in one sender
   task, the Deck forwards priority audio through its main connection loop,
   and one Deck media worker owns both the video decoder and the audio
   player. A blocked video send or decode therefore delays audio and input.
   A second QUIC stream alone does not fix that; independent workers do, and
   they need an agreed way to identify and own streams.
2. **A new capture provider.** A PipeWire/DMA-BUF provider must say what it
   actually delivers — format, plane layout, modifier, colour, GPU, copy cost —
   and each of Auto, Speed, Grading and HDR must decide from those facts
   whether it can be served truthfully. Without a shared description, each
   adapter would re-derive these rules and drift.

ADR 0013 keeps all displays' video on one ordered stream. This ADR keeps that
for video: the video lane carries the existing fair monitor mux. It separates
the other kinds of traffic from video, which ADR 0013 already did for audio
with the priority stream.

## Decision

### 1. Lane roles (`arcen_protocol::lanes`)

One authenticated QUIC connection on UDP 18444; QUIC assigns stream IDs and a
fixed-size preface declares each stream's role.

| Role | Stream | Opened by | Payload flows | Framing after preface | Send priority |
| --- | --- | --- | --- | --- | --- |
| `control` | bidi | Deck | both | existing session messages | 20 |
| `video` | uni | Pier | Pier → Deck | `u32` BE length + binary media frame | 0 |
| `audio_output` | uni | Pier | Pier → Deck | same (the existing priority-audio framing) | 10 |
| `input_cursor` | bidi | Deck | both | existing session messages | 30 |
| `microphone` | uni | Deck | Deck → Pier | `u32` BE length + binary media frame | 10 |
| `bulk_aux` | bidi | Deck | both | existing session messages | −10 |

Every binary frame type maps to exactly one lane
(`LaneRole::for_frame_type`). Audio keeps the priority-stream value 10
(`quic::AUDIO_PRIORITY_STREAM_PRIORITY`), and the media lanes reuse that
stream's length-prefixed framing rather than a new one. Frame bounds reuse
`BoundedTransportPolicy` and the existing audio/microphone limits
(`arcen_transport::lanes::lane_max_frame_bytes`).

Target concurrent-stream limits follow from the table plus one replacement
stream of headroom per shape the peer uses
(`arcen_transport::lanes::{PIER,DECK}_LANE_STREAM_LIMITS`):

| Endpoint grants its peer | bidi | uni |
| --- | --- | --- |
| Pier (Deck opens control, input, bulk; microphone) | 4 | 2 |
| Deck (Pier opens video, audio) | 0 | 3 |

`arcen_transport::quic::lane_transport_config(policy, endpoint)` grants these
counts for the lane carrier. The single-stream `recommended_transport_config`
keeps 1/1 until a product switches to the lane dispatcher;
raising limits alone would only admit streams nothing accepts.

### 2. Lane preface v1

36 bytes, fixed: magic `ARLN`, version `u16`, role `u8`, reserved flags `u8`
(zero), attachment generation `u64`, lane generation `u32` (nonzero), and a
16-byte binding. The control preface carries zero attachment generation and
binding; every other role must carry both. A reader reads the 6-byte header
first, so a legacy or other-version peer is reported before more is read.

After successful authentication the Pier generates a random binding for the
attachment and sends `lane_attachment` on the control lane (generation,
binding as 32 lowercase hex digits, granted roles). That binds lanes to an
authorization result, not to TLS establishment, and reuses the existing
direct-session authentication rather than adding an identity system. The
binding is redacted from `Debug` and never logged.

### 3. Protocol revision

Lanes are a breaking change to the transport layout. Deck and Pier are updated
together; **no legacy single-stream multiplexing is retained**.

- The lane preface carries `LANE_PROTOCOL_VERSION = 1`; any other value is
  refused with `UnsupportedVersion { received, supported }`.
- ALPN stays `arcen-quic-v1`, so an incompatible pairing completes TLS and can
  be told apart from a non-Arcen server, then is reported in application
  terms.
- A lane Pier that reads the legacy `arcen-direct-v1\0` preface closes with
  application code `0x41524C01` and a human-readable reason naming its lane
  version, which a pre-lane Deck prints. A lane Deck recognises the legacy
  Pier's existing `invalid direct stream preface` close (code 0).
  `PeerTransportMismatch::message` gives both sides one operator message:
  update the Deck and the Pier to the same release.
- `wire::PROTOCOL_VERSION` moves 4 → 5 in the same change that switches the
  product adapters to lanes, not in this contract change: no
  byte on the wire differs yet. The transport capability token stays
  `transport:quic-v1`; the lane version, not the hello, carries the layout.

### 4. Lane ownership and failure (`arcen_transport::lanes`)

`LaneRegistry` is pure state per attachment and connection, compiled without
the `quic` feature. Before any payload reaches a worker it refuses a
non-control lane before the control lane or before authorization; the wrong
shape or opener (wrong direction); a role outside the grant; a stale or
foreign attachment generation or binding (constant-time comparison); a
second lane with the open generation; and a lane generation not newer than
the last.

QUIC has no order between streams, so two arrival races are part of the
contract rather than errors:

- **Supersession.** An opener that resets generation *g* and opens *g+1* may
  have *g+1* arrive first. A peer-opened lane whose generation is newer than
  both the open one and every generation seen replaces it. The admission
  carries a `SupersededLane`: the replaced generation plus the role's
  recovery side effects from `LaneRecoveryPolicy` (reset the old stream;
  release held input for input/cursor and control; request a keyframe for
  video). **The owner must apply those steps whenever a lane is superseded**,
  before the new lane carries payload, because the late failure report for
  *g* is `Superseded` and carries nothing. Supersession is not a failure and
  consumes no recovery budget; audio and microphone state are untouched by
  a video supersession. A locally opened lane never replaces its own open
  lane; the owner reports the failure first.
- **Early Pier lanes on the Deck.** The Pier opens video and audio as soon as
  it has sent `lane_attachment`, so they can reach the Deck before that
  control message. While the Deck's control lane is open but not yet
  authorized, it holds such lanes unread, at most one per role
  (`MAX_PENDING_LANES_PER_ROLE`; a newer generation replaces the held one
  and reports the same `SupersededLane` steps, which are conservative for a
  stream that was never read).
  `authorize` validates each held lane in full — grant, attachment
  generation, binding, lane generation — and returns it admitted or refused.
  Ending the attachment or failing the control lane drops held lanes; the
  owner resets their streams. The Pier never holds a lane before
  authorization: Deck-opened lanes before authorization are refused.

A failure is lane-scoped. The registry returns one action:

| Role | Consecutive failures recovered | Then | Also |
| --- | --- | --- | --- |
| control | 0 | end attachment (existing reconnect) | release held input |
| video | 3 | end attachment | replacement starts with a keyframe |
| audio output | 3 | disable visibly | — |
| input/cursor | 3 | end attachment | release held input every time |
| microphone | 3 | disable visibly | consent generation untouched |
| bulk aux | 2 | disable visibly | — |

Write failures, partial framed writes, oversize and malformed frames require
a stream reset, never a clean finish, and the replacement lane must use a
newer generation; written bytes are not resent. A failed lane is never
replaced by another role's stream: `check_send` reports the role unavailable
and offers no alternative. A successful delivery (`mark_healthy`) clears the
count. Video recovery leaves audio and microphone state and generations
unchanged.

### 5. Capture-surface evidence (`arcen_media::video::capture_surface`)

`CaptureSurface` describes one delivered buffer as data: output identity and
`RegionGeneration`, coded and visible geometry, pixel format, per-plane
buffer/offset/stride/size, memory kind with an opaque 64-bit DRM-style
modifier, source colour, timestamps, damage, cursor representation, GPU
identity (PCI address, render-node minor, vendor) and copy path.
`CaptureSurface::validate` checks it against what the importer proved
(`CaptureImporter`). Unknown never satisfies a requirement:

- padded strides are accepted; short strides, truncated planes, planes past
  their buffer, overlaps and arithmetic overflow are refused;
- every DMA-BUF and host-memory buffer object must have a known size; an
  unknown size is refused (`BufferSizeUnknown`). Only opaque GPU device
  memory, whose importer validates the allocation itself, may omit it;
- the implicit modifier is never importable; a vendor modifier needs the
  importer's proof; host-memory paths need a linear layout;
- a zero-copy import or same-GPU conversion requires a proven same GPU;
  unknown is refused, a different GPU must be reported as cross-GPU;
- stale or future generations, foreign outputs, non-increasing sequences and
  a colour change within an epoch are refused;
- missing, excessive or out-of-bounds damage becomes full damage, matching
  Keel's `DamageMetadataStatus::Unknown`. A producer that never reports
  damage (GNOME 40 Mutter ScreenCast) is `DamageEvidence::Unknown` on every
  frame. A size-0 cursor-only buffer is cursor metadata, not a surface.

Input capability is already expressible in `arcen-input`: a provider without
pen support reports `InputCapabilities.pen = Unavailable`, which never
authorizes typed pen.

Copy cost is per path (`ZeroCopyImport`, `GpuConvert`, `HostCopy`,
`CrossGpu`). `CaptureBackend::PipeWire::zero_copy()` is deliberately left
`false`; it still describes the host-memory experiment.

Leases (`arcen_media::video::capture_lease`, added with GPU import): a capture
callback pushes validated buffers into a bounded latest-wins
`CaptureMailbox`; superseded buffers return to the producer at once with their
damage carried forward (`DamageEvidence::union`), and a `CaptureLease`
returns its buffer only when the import worker drops it. Buffer handles are
opaque adapter tokens.

### 6. Preset admission (`arcen_media::video::capture_admission`)

`admit_capture_source` dispatches to one function per preset and returns
`Admit`, `Downgrade` (served pipeline reported) or `Unavailable`:

- **Auto / Speed:** eight-bit SDR source family only; a wide capture is
  refused rather than routed through the fast path. Unknown SDR colour on an
  eight-bit RGB source is admitted with `colour_assumed`, the existing
  default for unvouched desktops; PQ/HLG/linear eight-bit is refused. NV12
  needs authoritative limited BT.709. Speed on a copying path without an
  authoritative rate measurement is visibly served as Auto; a measured rate
  below 60 caps the ceiling and says why.
- **Grading:** wide format, authoritative native-wide precision, and
  authoritative transfer and primaries. RGB10 SDR passes through; RGB10
  PQ/BT.2020 uses the shared PQ-to-SDR conversion; linear half-float uses the
  scRGB-to-SDR conversion. Up-converted or eight-bit content is refused.
- **HDR:** additionally an authoritatively active HDR output. RGB10 PQ passes
  through (never the scRGB-to-PQ transform); linear half-float needs an
  authoritative SDR white. Otherwise HDR is downgraded to Grading only when
  Grading itself is admissible, else unavailable with both reasons.
- Software encoders follow `software_fallback_decision`. The encoder must
  serve the complete contract for some codec in the pipeline's ladder,
  checked through `resolve_media_plan`.

### 7. Desktop-provider selection (`arcen_session::desktop_provider`)

`capture.provider` is `xorg` (default) or `wayland`; any other token fails
configuration loading. Every Pier validates it with
`validate_desktop_provider_for`: Windows and macOS refuse any value (the key
applies only to the Linux Pier), and the Linux Pier refuses `wayland` until a
build contains the Wayland adapter ("not available in this build yet"), so an
operator's request is never silently served by Xorg. Xorg is selected without
consulting Wayland. Wayland requires every separate fact — compositor,
session service, PipeWire, render-node access, GPU import, input injection —
to be proven; otherwise selection fails naming each missing or unknown fact.
There is no automatic migration and no silent fallback to Xorg.

### 8. Telemetry

Append-only events 2000–2005: `LANE_REFUSED`, `LANE_FAILED`, `LANE_STARVED`,
`PRESET_ADMISSION_DECIDED`, `DESKTOP_PROVIDER_SELECTED`,
`DESKTOP_PROVIDER_UNAVAILABLE`. `CAPTURE_PATH_SELECTED` (1105) gains optional
copy-path, memory, modifier-class, GPU-match, source-colour, precision and
cursor fields. Field values are the stable tokens above.

## Consequences

- Product adapters (QUIC runtime, lane dispatcher, capture leases, session adapter)
  implement these contracts; until then no lane is opened and no admission
  runs. Each Pier's configuration loader already validates
  `capture.provider`; the Linux Pier flips `wayland_adapter_built` when the
  session adapter lands.
- Pre-lane and lane builds cannot interoperate. The failure is explicit and
  names the fix.
- Separate lanes remove video-induced blocking, not shared congestion or
  network outages; bounded queues and `keep_send_window_interactive` remain
  required.
- Per-monitor video lanes, datagrams, and new USB functionality stay outside
  this decision.

## Amendment 1 (2026-10-17): lane runtime decisions

Recorded with the shared lane runtime (`arcen_transport::quic::LaneConnection`,
the QUIC runtime); products adopt it with the lane dispatcher.

1. **Session-message framing.** Control, input/cursor and bulk-aux lanes carry
   pre-encoded session frames through the runtime's `LaneSender`: the owner
   encodes each session message once and queues the complete frame as one
   record, which the lane's writer task writes verbatim. Its bounded queue,
   back-pressure (`LaneSendError::Full` returns the record, so no input edge
   is dropped silently) and failure handling therefore apply to these lanes
   as to media lanes. The products do not run a WebSocket codec over
   `LaneDuplex` for these lanes; the duplex remains for future byte-stream
   use and honours lane cancellation.
2. **Flooding interactive lanes share the audio priority.** QUIC priority is
   strict, so while the input/cursor (30) or control (20) stream has bytes
   buffered nothing of lower priority is sent. On loopback, an input flood
   held the same-direction microphone lane back by 96–255 ms. A lane of
   either role that writes more than 64 KiB within 100 ms
   (`INTERACTIVE_LANE_BURST_BYTES`, `INTERACTIVE_LANE_BURST_WINDOW`; about
   5 Mbit/s, far above real input and control traffic) is moved to the audio
   priority (10), where QUIC's per-stream fairness shares the path
   round-robin, and returns to its table priority after a window within the
   budget. Order and queue bounds are unchanged and nothing is dropped. With
   this rule the microphone's worst one-way delay under the same flood was
   below 10 ms. The table in section 1 remains each role's normal priority.
3. **Provisional bounds.** Per-role queue sizes and byte bounds
   (`LaneQueueBounds::for_role`), starvation thresholds and the burst
   threshold above are provisional until lab qualification on real
   networks; they may change without amending this ADR, and the change is
   reported with its measurements.

## Amendment 2 (2026-10-17): product adoption

Recorded with the product switch to the lane dispatcher: the Linux, Windows and
macOS Piers and the macOS Deck run every session on lanes and
`wire::PROTOCOL_VERSION` is 5.

1. **Session records.** Control, input/cursor and bulk lanes carry
   `arcen_protocol::lanes::SessionRecord`s: `u32` BE length, one kind byte
   (text 1, binary 2, close 3, ping 4, pong 5), payload; a close payload is a
   `u16` BE code and a UTF-8 reason of at most 123 bytes. The length prefix is
   the media lanes' one, so one cancel-safe reader serves every lane. The
   messages themselves are unchanged.
2. **Message-to-lane map.** `lane_for_session_text` sends every input event
   (keys, pointer, scroll, pen, gestures, text commit, held-state reset,
   region input) and the Pier's `cursor_shape` to input/cursor;
   `clipboard_data` to bulk; every other JSON message, including
   `lane_attachment`, to control. Binary frames follow
   `LaneRole::for_frame_type`. A message received on another lane is
   dropped (control, bulk) or fails that lane (input); it is never
   re-routed. The bulk lane's frame bound is the control bound (1 MiB), so
   clipboard senders fill a chunk to `CLIPBOARD_CHUNK_PAYLOAD_BYTES`, which
   makes the whole session record exactly 1 MiB; receivers still accept a
   payload of up to `CHUNK_BYTES`.
3. **Shared product layer.** `arcen_transport::quic::session_lanes` provides
   carrier setup for both endpoints (each runs
   `keep_send_window_interactive`), the grant and its acceptance,
   `SessionLaneSocket` (the control lane as the session `Sink`/`Stream`;
   adds `futures-core`/`futures-sink` under `quic`), `LocalLaneWriter`,
   `PeerLaneReader`, `LaneSenderSlot` and `follow_lane_exit`.
   `arcen_input::InputLaneOutbox` keeps input edges in order under
   back-pressure; a trailing absolute or region position is replaced by the
   next one of the same kind, trailing relative deltas are summed (newest
   sequence kept), and relative and absolute motion are never merged.
   `PeerLaneReader` reports a failed lane at once as `PeerLaneEvent::Lost`
   with its steps (release held input, request a keyframe), so held input is
   released before the peer reopens, not when the replacement arrives.
4. **Grant timing.** The Pier sends `lane_attachment` after authentication
   and before `server_hello`; attachment generations are monotonic per Pier
   process. The Deck consumes the grant inside its control socket.
5. **Visible audio disable.** An audio-output lane that exhausts its
   recovery sends `audio_stream_result` with the new reason
   `lane_unavailable`; a microphone lane that does so stops capture with
   `microphone_stream_stop` (`capture_failure`).
6. **Lane loss.** A lane exit caused by connection loss reports
   `LaneExit::ConnectionLost` with the mismatch classification rather than an
   ended attachment, so a peer on another layout is named.
7. **Deck audio.** Audio-output frames go to a dedicated audio-worker thread
   that owns the CoreAudio player; the video media worker no longer decodes
   or plays audio.
8. **Piers whose session runs behind a local socket.** The Windows broker
   and the macOS service (and macOS `serve`, in process) use the shared
   `arcen_transport::quic::PierLaneRelay`: the agent's single local message
   socket is demultiplexed onto the lanes. Control messages stay on the
   control lane through the caller; video and audio go to their own writer
   tasks and routing never waits on them. A full video backlog
   (`RELAY_VIDEO_BACKLOG`) drops the frame, holds every broken monitor chain
   until its keyframe and asks the agent for one (`request_full_frame`,
   spaced 250 ms); a full audio backlog drops the frame; cursor shapes go
   newest-first on the input lane; clipboard/HID/USB go through a bounded
   bulk backlog that drops and counts on overflow. A dropped clipboard
   transfer is abandoned visibly: the rest of it is dropped and the Deck gets
   the new control message `clipboard_transfer_failed` (sequence, reason
   `bulk_lane_full` or `bulk_lane_unavailable`). The notice has no order with
   the bulk lane, so the Deck scrubs only that transfer if it is in flight,
   never a newer one, and refuses a late offer for it as stale
   (`ClipboardReassembler::abandon`); it tells the user. Deck input is forwarded in order and never
   dropped, and a replaced, failed or ended input lane is followed at once,
   on the same ordered channel, by `key_reset_modifiers` with reason
   `input_lane_recovery` (`RELEASE_ALL_HELD_INPUT_REASON`): the agent
   releases every held key, button, pen and region focus state (Windows:
   the session-end release; macOS: `release_all`), while a Deck's own
   `key_reset_modifiers` still releases keys only. The
   Windows broker grants at the start of each attachment (after its own
   authentication); the macOS relay grants after the agent's successful
   `auth_result`, or before its `server_hello`, whichever comes first. The
   macOS agent's audio side channel feeds the audio lane for every relayed
   session.
9. **Not yet done.** Lane telemetry sinks in the products
   (`ObservabilityLaneSink` needs the session id at accept time); the
   agent's local socket on Windows and macOS is still one ordered stream
   between the agent and the relay (local, never the network; the macOS
   agent sends audio on its own side channel, the Windows agent's writer
   serves audio first); qualification on real networks and hardware.
   `ClientHelloMsg`'s `audio_priority_stream_v1` stays in the vocabulary for
   pre-lane peers; the Deck sends `false` and no lane Pier reads it.
10. **Disabled bulk lane.** Clipboard and Hard USB stop for the session;
    the session goes on. The Deck refuses further bulk sends quietly,
    clears its clipboard state, stops its Hard USB responder and shows a
    status hint. The Linux Pier sends Hard USB frames through its own
    bounded writer task (never from the control dispatcher); when the bulk
    lane is disabled or cannot take a frame in time it stops the USB bridge
    and sends `tablet_mode_result` with mouse compatibility, which the Deck
    also honours mid-session.
11. **Deck audio decision.** The Deck's session loop hands
    `audio_stream_result` to the audio worker on receipt, in order with the
    audio lane's frames; the player drops frames until it arrives instead of
    guessing their codec.

## Amendment 3 (2026-10-17): capture admission before READY

Recorded with the Wayland Auto and Speed compositions.

1. **Admission is stated, not inferred.** A provider that learns its source
   facts only after negotiation and a first frame (the `PipeWire` provider)
   decides admission in the capture process with section 6's functions. It
   states the outcome before `READY` as `[capenc] ADMISSION version=1 …`,
   the serialisation of `PresetAdmission` plus the first frame
   (`arcen_media::video::CaptureAdmissionNotice`). The parent checks it with
   `check_request` (same preset and requested rate, a permitted answer, no
   more than the requested rate, same codec, stated cursor truth, same
   session) and only then accepts a `READY` at the admitted rate; without a
   notice `READY` stays strictly equal to the request. An `unavailable`
   notice may be followed only by `UNAVAILABLE` or the child's exit, and a
   second notice is refused. The notice also carries the first-frame
   readiness evidence and the route that encoded it
   (`FrameEvidence::import`).
2. **The served contract runs.** `AdmittedCapture::epoch` gives the epoch the
   served contract's cadence, priority, intent and keyframe policy, so a
   downgraded request never runs with the requested contract's settings.
3. **Cursor truth comes from the stream.** `ScreenCastCursorMode` maps to
   `CursorEvidence` for the whole stream, and `CursorEvidence::authority`
   maps evidence to cursor authority once; a source with no cursor has no
   authority and is refused rather than served without a cursor.
4. **Rate evidence is measured.** `SustainedRateMeter` keeps the best rate
   held over three consecutive delivering seconds, so a still desktop that
   sends nothing is not read as a slow source.
5. **The request bounds the rate.** `AdmissionRequest::max_fps` carries the
   requested frame-rate ceiling: the admitted ceiling is the lowest of the
   contract's, the request's, the encoder's and a qualified source rate,
   and asking for less than the contract is not a degradation.
6. **Capture route, not GPU import, for Auto/Speed (amends section 7).**
   `WaylandCapabilityFacts::capture_route` is proven when a truthful route,
   including a labelled host copy, delivered an encoded first frame; it is
   the selection fact the eight-bit Auto/Speed provider needs.
   `gpu_import` keeps its meaning (compositor buffers imported on the GPU,
   `gpu-convert` or `zero-copy-import`), is still reported, and is not
   required for Auto/Speed; a host copy never proves it. Speed's rate
   qualification stays separate. Session proof of either fact never changes
   start-time selection, which reads host facts.

## Amendment 4 (2026-10-17): automatic provider selection and the host qualification record

Recorded with the Pier's Wayland server path.

1. **Session-proven facts come from a record.** The capture route, input
   injection and Speed's source rate can only be proven by a session, but
   selection runs at start. `arcen_session::wayland_qualification` defines a
   versioned record (`schema_version` 2) that a root-only command
   (`arcen-pier wayland qualify --write`) produces by running a real
   headless session through the product machinery. At start the Linux Pier
   reads it from `/var/lib/arcen/wayland-qualification.json` (a regular file
   owned by root and writable by root alone, in such a directory) and
   `QualifiedHost::apply` overlays its capture route, GPU import and input
   injection on the live probe; a fact the live probe proved missing is
   never hidden. A missing, unreadable, malformed, other-schema or stale
   record refuses Wayland with a message naming the command; there is still
   no fallback to the other provider.
2. **Validity is the exact stack.** A record is stale when the Pier version,
   source revision or executable digest, the compositor (kind or version),
   the `PipeWire` library version, or the GPU driver (version, its DRM
   module appearing or disappearing, or that module's `modeset`) differ
   from what it was proven with.
   The operating system and kernel are recorded for operators only.
3. **Speed's rate is per output.** The record carries the best sustained
   source rate measured per output size and refresh rate on the route that serves Speed
   (capenc's `source_fps` while the session's pointer moves, through
   `SustainedRateMeter`). A Speed request is admitted on it
   (`capture-rate=`) only for that exact size and refresh rate, and only
   when it reaches 95 %
   of Speed's ceiling (`speed_minimum_fps`, 57 fps for 60); any other size
   or a slower source is visibly served as Auto. The qualification's own
   measurement runs claim no rate.
4. **Grading and HDR are recorded unavailable** until a Wayland ten-bit or
   HDR composition exists and is proven; a Wayland session serves a Grading
   or HDR request as Auto, visibly (served label, eight-bit truth in the
   hello, a logged downgrade reason), rather than failing it.
5. **Selection is automatic (replaces section 7's explicit opt-in).**
   `capture.provider` is `auto` (default), `xorg` or `wayland`
   (`ProviderPreference`); `select_desktop_provider` reads
   `HostDesktopFacts`. Under `auto` an installed Xorg server selects Xorg
   without consulting Wayland, so no Xorg installation is ever migrated;
   without Xorg, Wayland needs every fact including an accessible DRM
   render node, and a host with neither Xorg nor DRM is refused with the
   driver to install (DRM/KMS; for NVIDIA `nvidia-drm` with `modeset=1`).
   Arcen never changes driver configuration. An override that cannot be
   honoured refuses to start (`ProviderRefusal`). Decisions use facts,
   never distribution names.
6. **Per-session proofs remain.** Every session still waits for its own
   first frame (`ADMISSION`) and proves its input socket; the record never
   stands in for them.

## Amendment 5 (2026-10-17): verified GPU import and the paced Speed output

Recorded with capenc's `gpu-convert` route and the 120 Hz Speed output
(GPU import and the paced Speed output).

1. **GPU import is proven by pixels, not by a frame.** A record's
   `CaptureRouteProof` gains `gpu_import_verified_frames` (`serde`
   default 0, so schema 2 is unchanged and an older record reads as
   unverified). `proven_gpu_import` requires a GPU route
   (`gpu-convert` or `zero-copy-import`) that encoded frames *and* at least
   one GPU-imported frame that matched a host-copy reference of the same
   desktop (capenc `wayland-capture-probe verify-gpu-import`: at most 0.1 %
   of pixels may differ, on a reference whose content distinguishes a
   red/blue swap). A host copy never proves it.
2. **DMA-BUF is offered only on a verified route.** The Pier passes
   `gpu-import=verified` to a Wayland capenc only when the record proves GPU
   import; capenc then offers DMA-BUF (with the modifiers its importer
   reports) only if its importer opened on the encoder GPU, passed its
   GL-to-CUDA self-test and the compositor is the same GPU. Otherwise the
   epoch offers shared memory and logs why. An import failure withdraws
   DMA-BUF for the epoch (the producer renegotiates to shared memory); no
   frame is encoded from the failed import.
3. **Paced sources get headroom (`arcen_media::video::capture_rate`).** A
   compositor that paces its capture by the output refresh delivers well
   below the rate a consumer asks for when the request equals it.
   `paced_source_offer_fps` (twice the served rate) is what Speed asks the
   producer for, and `paced_output_refresh_hz` (at least twice the served
   rate, never below the display's) is the refresh a Speed session's
   virtual output is created at. Measured on Mutter 49; whether a stack
   then sustains Speed is still the record's per-output measurement.
4. **The output is decided once per session.** The Linux Pier creates a
   headless session's output at the paced refresh only when the
   authenticated request resolved to Speed and the record qualifies Speed
   at that size and refresh; every other session keeps the display's
   refresh. Qualification creates its session the same way, so its Speed
   rate is recorded for the output refresh the Pier will create. A later
   Speed request on a session created at the display's rate is visibly
   served as Auto unless that rate is itself qualified.

## Amendment 6 (2026-10-17): liveness, capture restart and Wayland output generations

Recorded with output reconfiguration and recovery.

1. **Lane carrier liveness** (`arcen_transport::liveness`). The lane
   transport config sends a QUIC keep-alive every 1 s and closes a
   connection idle for 10 s (was 20 s / 30 s); QUIC applies the smaller
   peer value, so every Pier and Deck of this release gets it. A Pier that
   finds its capacity-one session slot held may, for a newcomer
   authenticated as the **same user**, prove the incumbent connection's
   peer silent (no datagram for 3 s while the keep-alive runs,
   `quic::IncumbentAttachment`) and close it with application code
   `0x41524C02`, then wait up to 12 s for its teardown. The displaced
   attachment learns it was displaced (`IncumbentRegistration::displaced`,
   awaitable) and hands the desktop over instead of holding it for a
   resume: it drains its resume slot, releases the session slot and keeps
   the desktop for the newcomer. An incumbent whose connection already
   closed on its own (its idle timeout, before or during the check) is
   marked the same way and hands over from its reconnect wait; the mark is
   set only if the record still names the same registration and
   connection after the check, so a resumed attachment is never marked. A
   newcomer that arrives after the incumbent detached takes it over at
   authentication instead (decided 2026-10-18): a fresh
   sign-in that passes full PAM authentication as the **same user** makes
   the detached owner hand the desktop over (`OwnerCommand::TakeOver`,
   decided by the pure `arcen_session::direct_reconnect::fresh_sign_in`);
   the old resume grant is retired and its holder told "this session was
   taken over by a newer sign-in of the same user". "Same user" is the
   account the operating system resolved after authentication — canonical
   name and uid both equal (`AccountPrincipal`) — never the name the Deck
   typed, so aliases (`Alice`, `alice@corp.example`, `CORP\alice`) reach the
   same account and a rewritten `PAM_USER` cannot borrow another's. The
   incumbent record on the liveness path is keyed the same way. No
   incumbent is judged while a resume is being validated, and moving the
   registration to a resumed connection clears any displacement mark. Another user is refused
   after authenticating; an attached session is never taken over by a
   sign-in (only the liveness proof applies); with a sign-in disclaimer
   configured only resume continues a detached session.
   A live incumbent and another user's are never probed or closed. The
   Linux Pier implements the displacement; the Windows and macOS Piers
   admit before authentication and use the shorter idle timeout only.
2. **Capture restart** (`arcen_media::video::CaptureRestartBudget`). A
   single capture epoch that ends on its own is replaced inside the
   attachment, keyframe first, at most three times per 60 s; audio,
   microphone and input are untouched. Past the budget the attachment ends
   with a stated reason.
3. **Wayland output generations** (`arcen_session::wayland_outputs`). The
   output mode is decided from the compositor's version; in the
   remote-monitor mode (Mutter 49+) every resize, re-placement or topology
   change is one output transaction that must name exactly the next
   generation, and capture hand-offs and input coordinates carry that
   generation. A stale generation is refused, never applied.
4. **Lane telemetry in products (partly closes Amendment 2, item 9).**
   `ObservabilityLaneSink::bind` moves a sink to a new lifecycle context.
   The Linux Pier attaches one to every connection's lane runtime under a
   connection-scoped id and binds the session's log id and user after
   authentication; the Deck reports under its session log id. The Windows
   broker and the macOS service do not attach a sink yet.

## Amendment 7 (2026-10-18): orderly session close

Recorded with the 0.17.0 fix that drains final records before a lane closes
(before it, every Pier refusal reached the Deck as `control lane ended:
ConnectionLost`, and a Deck's clean close reached the Pier as a transport loss).

1. **Cause.** Dropping a `SessionLaneSocket` finished the control lane,
   waited only until its writer had handed the bytes to QUIC, and closed the
   connection. QUIC's `CONNECTION_CLOSE` abandons stream data the peer has
   not acknowledged, so the last records (an `auth_result` refusal, a close
   reason, the Deck's close record) were lost in most runs (reproduced
   in-process: 16–17 of 20 refusals and 5–6 of 20 Deck closes lost). The
   socket's sink close also sent no close record, unlike the WebSocket it
   replaced, so the Pier read a bare end of stream as a transport loss.
2. **Orderly close.** Closing the socket's sink sends a close record unless
   one was sent, then finishes the control lane. Dropping the socket closes
   in the background within `SESSION_CONTROL_LINGER` (2 s): finish the
   control lane; wait until QUIC confirms the peer received every byte of it
   (`LaneSender::delivered`, from `SendStream::stopped`); wait until the
   peer finished its own control lane or closed the connection; then end
   the attachment and close with `SESSION_CLOSED_CLOSE_CODE` (`0x41524C03`,
   reason `session closed`). Each side's wait ends on the other's finish, so
   two sides closing at once do not wait on each other; a peer that never
   answers holds the connection for the bound only. Other lanes are not
   delayed.
3. **Reading after a close.** A lane cancelled because its connection closed
   first hands out what QUIC received before the close (a closed connection
   never blocks a read); a superseded, disabled or failed lane is never
   drained. The socket's stream ends cleanly after an orderly peer close
   (`closed_in_order`). An abrupt loss still surfaces as
   `LaneExit::ConnectionLost`.
4. **Control outlives `LaneConnection::end`.** Ending the attachment resets
   every lane except an open control lane, which its socket closes as above;
   the end of the connection still cancels it.
5. **Pre-lane Deck.** A lane Pier that reads the legacy `arcen-direct-v1`
   preface writes one unmasked WebSocket close frame (status 1008, reason
   `legacy_deck_refusal_reason`) on that stream, which the pre-lane Deck
   prints as "host closed the connection during setup: …", waits up to 1 s
   for its acknowledgement and then closes with the version-mismatch code as
   before. `accept_session_lanes` reports `SessionLaneSetupError::Mismatch`
   and every Pier logs it at warn.


## Amendment 8 (2026-10-18): Speed evidence without operator sizes, the runtime guard, development records and automatic re-qualification

Recorded with qualified Speed admission on Wayland (inferred admission,
schema 3; the service re-qualifies itself; development records).

1. **Why 60 Hz looked like 40 fps.** Mutter's ScreenCast source drops a frame
   whose timestamp lies inside one interval of the consumer's negotiated
   `maxFramerate` after the previous one (`meta-screen-cast-stream-src.c`,
   49.4). A *monitor* stream (`RecordMonitor`, the `--virtual-monitor` lab
   harness and GNOME 40's startup monitors) cannot negotiate above the
   monitor refresh, so presentation jitter drops frames (36–41 fps at 60 Hz).
   A `RecordVirtual` stream on Mutter 49 reports no refresh and its own
   consumer's `maxFramerate` applies. Upstream fixes the jitter for every
   stream in MR !5358 ("Overhaul 'too early' computing", merged to `main`
   2026-10-14, milestone GNOME 51, after 51.1; not on the 49 or 50 stable
   branches, and not in Rocky/RHEL 10.2's `mutter-49.4-4.el10_2`). Measured
   on Rocky 10.2 / Mutter 49.4: a `RecordVirtual` output at 60 Hz delivered
   59.3–59.7 fps of a 60 fps client at 1080p, 1800x1130 and 4K, with
   `maxFramerate` 60 or 120, under Mutter and GNOME Shell. Arcen does not
   patch compositors; it measures.
2. **Lowest refresh that holds Speed.** Each Speed measurement records the
   Deck display refresh, the output refresh, the codec and the route
   (record schema 3). A session created for Speed gets the lowest refresh
   with evidence (the display's own on Mutter 49's virtual outputs, the paced
   refresh of Amendment 5 only where that alone holds), so a session created
   for Auto can serve Speed on a later attachment.
3. **Inferred admission.** A size without its own measurement is admitted on
   the smallest qualifying measured output at least as wide and as tall, at
   the same refresh, codec and route — never upward or across those axes —
   and labelled `inferred:<W>x<H>` in the Pier log, capenc's argv
   (`capture-rate-evidence=`) and the `ADMISSION` line (`rate_evidence=`).
   A rate at or above the floor admits the contract ceiling. This host's own
   "no" wins: at the same refresh, codec and route, a failed measurement of
   the requested size, of a size it contains, or of a size between it and the
   anchor refuses the inference. A resize never upgrades an attachment's
   contract; Speed resized to a size without evidence continues as Auto
   (`served_pipeline` with the admission's reason).
4. **Displays are never lost.** A qualification measures operator
   `--output`s, a default ladder (1920x1080, 2560x1600, 3840x2160 at 60 Hz),
   up to four displays from a root-owned list of displays Decks requested
   Speed at without evidence (most recent first), and the previous record's
   displays that held Speed and are not covered by a larger qualifying one;
   at most ten, the rest kept in the requested list. Unknown `facts`
   sections in a record are ignored, so a newer build's record reads as
   stale and is re-qualified rather than refused as malformed.
5. **Runtime guard.** Every admitted Wayland Speed epoch on a copying route is
   judged on capenc's own per-second delivered and encoded counts (host side,
   never what a Deck received): among the first five seconds in which the
   source delivered at least the floor, three encoded below it trip the
   guard, and the attachment is re-served as Auto (`speed-runtime=below-floor`
   → shared `admit_speed_after_runtime_shortfall`, reason
   `speed_runtime_below_floor`, a `served_pipeline` message to the Deck).
   Seconds where the source delivered less are content (a still desktop, a 30
   or 50 fps video), not evidence, and are not judged. Not cached.
6. **Development records.** `capture.wayland_qualification_record` /
   `qualify --record` name another record with its own temporary account and
   scope; `--allow-running-service` is accepted only for such a record.
7. **Automatic re-qualification.** A record refused only as stale or of
   another schema, with no live blocker, is re-qualified by the Pier service
   at start (bounded, logged, attempt marker; a failure on the same stack is
   not retried for 6 h, an interrupted attempt is retried up to 3 times in a
   row; `capture.wayland_auto_requalify = false` opts out).
   Never for a missing or untrusted record.

## Amendment 9 (2026-10-18): Wayland software encode and the hosts without NVENC

Recorded with the software encoder fallback (before it, a Wayland host
without NVENC could not stream; an Xorg one could).

1. **A separate composition.** `linux-wayland-software`
   (`hosts/capenc/src/wayland/pipelines/{software,software_runner}.rs`) serves
   a request for `encoder=software-h264`. It resolves the request to Auto or
   Speed with the shared preset rule and admits it with that contract's own
   shared admission given OpenH264, so the shared software rule decides:
   Auto is served as Software, Speed is visibly downgraded to it, Grading and
   HDR never reach it. It offers shared memory only, never DMA-BUF, and never
   touches CUDA, EGL or NVENC; the Auto/Speed NVENC runner is unchanged.
2. **One OpenH264 session.** `arcen_media::video::SoftwareH264Session` owns
   what every Linux software path shares (contract bitrate bounds, retained
   I420 planes, selective conversion, keepalive/keyframe re-encodes, live
   rate rules, admission-probe pictures). The Xorg MIT-SHM loop and the
   Wayland composition both run on it; `SoftwareEncodeLoop` adds Keel's hash
   and idle cadence for sources that deliver a picture only on change.
3. **Source-drawn cursor.** A backend contract's `cursor_in_video` says
   whether that backend's capture path can put the cursor into its pictures.
   `BackendLimits::with_source_cursor` widens only that axis, only from
   capture evidence (`CursorEvidence::Composited`): a producer that drew the
   cursor needs no such ability, so OpenH264 states `cursor=host` for an
   embedded Wayland cursor. Xorg MIT-SHM never reports a composited cursor.
4. **Encoder geometry is admission.** A stream larger than the encoder's
   limits is refused as `EncoderGeometryExceeded` (`encoder_geometry_exceeded`)
   before codec selection. A Wayland output is created at its size before
   any encoder exists, so the Pier fits it to OpenH264's limits when the
   host's sessions are software (`Config::wayland_software_sessions`: an
   operator pin, or `auto` with `facts.encoder.native = missing`) and
   on such a host also fits a `display_update` and, at attach, an output an
   earlier epoch left larger (a resizable `RecordVirtual` output, before any
   capture of the attachment exists). A native host whose NVENC fails at
   spawn refuses an oversize software fallback by name instead of resizing
   the output under the attachment.
5. **Qualification knows the encoder.** `arcen-pier wayland qualify` probes
   the native encoder (`probe-v1`) before the capture run; a host without one
   qualifies with OpenH264 and records the schema-3 section `facts.encoder`
   (`native`, `capture_route_encoder`), optional per the schema-3 extension
   convention: absent means not proven (native tried first, nothing fitted),
   and an older build ignores it. Selection never required NVIDIA, only a render node a session can
   open; the advice now names Intel, AMD and virtio-gpu drivers too.
6. **Not yet.** Vendor-neutral hardware encode on Intel/AMD (VA-API, Vulkan
   Video) and hosts with no render node at all (Mutter can composite
   surfaceless) are designed follow-ups, not refusals of principle.

## Amendment 10 (2026-10-18): Wayland Grading and HDR

Recorded with Wayland Grading and HDR. Numbered after Amendment 8 (qualified Speed) and Amendment 9 (software encoder fallback).

1. **The compositor decides whether a wide route exists.**
   `arcen_media::video::compositor_fidelity` turns the compositor release
   and its colour facts into a route or a gap:
   - Mutter < 50 has no HDR screen sharing (mutter!4928), so the gap is
     `compositor_lacks_hdr_screencast`.
   - Mutter ≥ 50, whose virtual monitors offer no BT.2100 colour mode, gets
     `force-hdr`.
   - A future compositor whose virtual outputs offer BT.2100 gets
     `colour-mode`.
   The gaps are stable tokens. Their text is the reason a Deck sees.
2. **Precision is measured, never read from a format.** A known ramp is
   read back through the capture (`expected_ramp_code`, `measure_ramp`,
   `precision_from_ramp`):
   - Native-wide needs at least 90 % of the expected codes and a maximum
     error of one code.
   - 256 or fewer distinct codes means eight-bit pixels in a wide
     container.
   - A patch of three distinct channel codes must come back in order,
     because a grey ramp cannot reveal a red/blue swap.
   The proof is per pixel format and memory type (`RampProof::proves`,
   `FidelityProof::proves(format, memory)`, capenc
   `wide-proven=<format>@<memory>`). An epoch offers only proven formats
   and trusts precision only for the negotiated pair. Mutter's
   shared-memory screencast paints through an eight-bit path even when it
   negotiates a wide format, so a DMA-BUF proof never vouches for shm.
   Neither does a proof of one format vouch for another.
   An administrator's pinned eight-bit variant keeps a session out of the
   ten-bit contracts: it is served as Auto, with no route.
3. **Admission gains `SourceTransform::HalfFloatPq`.** It takes F16 PQ
   samples directly (HDR) or converts them to BT.709 SDR in shared code
   (Grading, `convert_rgba_f16_pq_to_packed_rgb10`). Every PQ arm needs an
   authoritative 203 ± 5-nit graphics white
   (`AdmissionReason::ReferenceWhiteUnsupported` otherwise).
4. **The capture hand-off may name a wide node**
   (`ARCEN_PIPEWIRE_WIDE_NODE`). It is used only by a Grading or HDR epoch.
   Auto and Speed never see it.
5. **The qualification record carries the proof.** `facts.fidelity`
   holds the route, the gap, the ramps and the SDR white, inside schema 3.
   It is optional, so earlier records still parse. `facts.grading` and
   `facts.hdr` become `available` only from it.
6. **Constraints.**
   - `force-hdr` sets `MUTTER_DEBUG_FORCE_HDR=1` in the session
     compositor's environment only. It is a debug switch, used without
     `--debug-control`.
   - Product qualification needs a GNOME ≥ 50 host.

## Amendment 11 (2026-10-18): host cursor images on the input/cursor lane

Recorded with the Wayland Deck-drawn cursor. Numbered after Amendment 9
(software encoder fallback) and Amendment 10 (Wayland Grading and HDR).

1. **A binary record on the input/cursor lane.** `FrameType::CursorUpdate`
   (`0x50`, `arcen_protocol::cursor`) routes to `LaneRole::InputCursor` and
   `ReliabilityClass::InputLowLatency`. It is the only binary record on that
   lane and flows host → Deck only; a Pier still fails the lane for any
   binary record from the Deck. One record is the whole current cursor
   (visible image or hidden, size, hotspot, output and generation, a
   fallback `CursorShapeKind`), so the Pier keeps only the newest
   (`arcen_input::cursor::CursorLaneOutbox`), like `cursor_shape`. A record
   is at most 36 bytes plus a 60 KiB PNG, inside the lane's 64 KiB frame.
2. **Images travel once, without acknowledgements.** Both ends run the same
   deterministic weighted LRU (`CursorImageCache`, 32 images, 2 MiB): every
   record delivered on the ordered lane touches or inserts one id on both
   ends, the Pier commits its mirror only after the lane accepted the
   record, and both ends clear it when the lane is reopened. The PNG is
   inlined only when the mirror lacks the id.
3. **Negotiation.** `AuthResponse.cursor_image_v1` (echoed in
   `ClientHelloMsg`) states that the Deck draws images; it is sent at
   authentication because a Wayland desktop fixes its stream's
   `cursor-mode` when it opens
   (`arcen_media::video::capture_handoff::resolve_stream_cursor_mode`:
   metadata only for a local-cursor Deck that draws images on a host whose
   qualification record proves `cursor_metadata`). `CursorModeResultMsg`
   gains `cursor_images` and a reason when the desktop's fixed mode answers
   a different preference. A Deck without the capability receives the
   `cursor_shape` names only (`default`/`hidden` for a metadata desktop).
   All fields are additive and omitted when false; `PROTOCOL_VERSION`
   stays 5.
4. **capenc framed-v2.** A capture that reports cursor events writes
   `u32_be length || u8 kind || body` records on stdout
   (`arcen_media::capenc_stream`): kind 1 is the access unit framed-v1
   carried, kind 2 a cursor event (on/off the output, unchanged, empty or a
   new straight-alpha sprite of at most 384²). capenc runs as the desktop's
   user, so the parent bounds every record before allocating it. framed-v1
   is unchanged for every other capture.

## Amendment 12 (2026-10-19): arrival pacing and the paced Xorg head

Recorded with the capture pacing and recovery fixes.

1. **A capture that delivers on its own clock is paced on arrival**
   (`arcen_keel::ArrivalPacer`). The eight-bit NvFBC loop used to decide
   once per contract tick. With the Xorg head refreshing at the served rate
   (60 Hz for Speed), arrival jitter put two frames into one tick interval
   and none into the next: capenc grabbed 60 new frames a second and
   encoded 40 (`emit_activity=40`, `pipeline_flush=20`), and the Deck showed
   33–43 fps against a 60 fps host page. Fresh content is now submitted as
   it arrives unless it is more than half an interval ahead of its slot; the
   next slot is one interval after the later of the slot and the
   submission, so the long-run rate never exceeds the contract and jitter up
   to an interval loses nothing. Keepalives and encoder pipeline flushes
   wait until half an interval past the slot and do not move it.
2. **A new Xorg desktop's head runs at the paced refresh.** The single-head
   Xorg EDID used to carry the Deck display's refresh (the Deck asks for at
   most 60 Hz; a 30 Hz panel capped every preset at 30). The Linux Pier now
   creates it at `paced_output_refresh_hz` for the requested preset's rate
   (120 Hz for Speed on a 60 Hz display, 60 Hz for a 30 fps preset on a
   30 Hz one), the same headroom a headless Wayland output gets; the
   display's own rate is kept when it is already high enough, when an EDID
   cannot describe the paced timing at that size (CVT-RB pixel-clock limit,
   e.g. 4K at 120 Hz), or when the request names no preset. A reattachment
   keeps the desktop's refresh. Multi-head Xorg plans are unchanged.
3. **Evidence** (Rocky 9.7 / GNOME 40 Xorg depth 24 / GRID V100D, the real
   Deck GUI over the Norway→London VPN, real-screen harness, Firefox page
   on the host): before, host `emit_activity` 40–46 of 60 captured and
   41–43 fps on glass; with the pacer on a 60 Hz head 60/60 encoded and
   59.5–59.7 fps on glass (2–5 page frames lost per phase instead of
   113–172); with the 120 Hz head the page ran at 120 fps, 60/60 encoded,
   58.9–59.7 fps on glass and no pipeline flushes in motion.

## Amendment 13 (2026-10-19): pen barrel-button capability

Recorded with the pen barrel-button work; written down at release.

1. **Field.** `InputCapabilitiesMsg.pen_barrel_buttons`
   (`InputCapabilityAvailability`, `#[serde(default)]`, so `Unknown` from a
   peer that predates it). It says whether the pen's barrel buttons reach the
   desktop, beside the existing per-axis pen fields.
2. **Who sets it.** The Linux Pier derives it from the pen truth of its
   device (`PenCapabilityTruth`): `unavailable` for an emulated stylus
   without a button interface. The Windows and macOS Piers set it with their
   other pen capabilities, and the Deck declares it for its own pen.
3. **Compatibility.** Additive; no `wire::PROTOCOL_VERSION` or
   `INPUT_PROTOCOL_VERSION` change. Older peers ignore it.

## Amendment 14 (2026-10-19): committed text on the input/cursor lane

Recorded with committed text input; written down at release. The wire format is in
`shared/protocol/WIRE.md` ("Committed text").

1. **Capabilities.** `InputCapabilitiesMsg.text_commit` (a client sends
   `text_commit` and understands the two notices; a host types it) and
   `text_commit_unicode` (a host can type characters its session layout
   lacks), both `#[serde(default)]`. `supports_text_commit_v1` is true only
   for `text_commit = available`; neither side sends a message the other
   did not advertise.
2. **Messages.** `text_commit` (client → host) gains `sequence` (omitted
   when 0) and `validate()`: non-empty, at most `MAX_TEXT_COMMIT_BYTES`
   (1024) UTF-8 bytes, no control characters or noncharacters. The host
   sends `text_commit_result` (sequence, delivered and refused counts, up to
   64 refused characters, a `TextRefusalReason` token, `unverified`) only
   when something was refused or typed without proof, and `host_keymap`
   (layouts, active layout, up to 128 keys) at attach and on every layout
   change.
3. **Lane.** All three travel on the input/cursor lane, ordered with the
   input they answer. `TextRefusalReason` parses unknown tokens as `unknown`.
   Text is never logged, only counts.
4. **Compatibility.** Additive; `INPUT_PROTOCOL_VERSION` stays 4.

## Amendment 15 (2026-10-19): a served-pipeline remedy for the user

Recorded with the per-desktop X11 depth; written down at release.

1. **Field.** `ServedPipelineMsg.detail: Option<String>`, omitted when
   absent. `reason` stays the stable machine token; `detail` is one sentence
   the Deck shows beside the downgrade, saying what the user can do. It is
   display text only and never parsed.
2. **First use.** The Linux Pier sends `EIGHT_BIT_DESKTOP_DETAIL` with
   `reason = eight_bit_source` when Grading or HDR reaches an Xorg desktop
   created at depth 24 (`video.x11_desktop_depth`): sign out and reconnect
   with Grading or HDR, or have the host pinned to depth 30.
3. **Compatibility.** Additive; absent from every older host, and an older
   Deck ignores it.

## Amendment 16 (2026-10-20): desktop admission requires polkit to treat the session as remote

Recorded with the remote-session polkit rule.

1. **Problem.** polkit's systemd backend calls a subject local when its
   logind session has a seat and ignores `Remote`; a session-less process is
   judged by its user's graphical session. An X11 desktop (a session on
   `seat0` at VT 1, activated by the launcher) and a native Wayland desktop
   (its own `seat-arcen-pen-…` seat) therefore gave their remote users the
   console's `allow_active` grants. Headless Wayland desktops are seatless and
   were never affected.
2. **One shared mechanism** (`arcen_session::polkit_remote`, pure): the
   `.policy` reader (`allow_any` per action), the logind session-record
   classifier (remote when `REMOTE=1`, `TTY=arcen`, or an Arcen seat), the
   rule text, the polkit version gate (JavaScript rules from 0.106), and the
   admission decision. The Linux adapter (`session/polkit_remote.rs`) only
   reads `.policy` files, writes the rule, and runs `pkaction`/`pkcheck`.
3. **The rule is static.** `/etc/polkit-1/rules.d/05-arcen-remote-session.rules`
   names no session: for a seated subject it reads the session's logind
   record through `polkit.spawn` (JavaScript sees no logind properties),
   caches remote sessions, and answers each action's `allow_any` (falling
   through when that is `YES`), or `AUTH_ADMIN` for an unknown action; an
   unreadable record is remote and logged once. Site restrictions for remote
   seated desktops must be numbered `00`–`04`, since later rules no longer
   reach those sessions for the actions it answers. It is written by the installer and
   rewritten by the Pier at start (atomic, only when it changed), never per
   sign-in; it sorts before every distribution rule. It replaces the native
   seat branch's `49-arcen-native-seat.rules`, which it removes when found.
4. **Admission, fail closed for seats.** A refusal the polkit state alone
   decides (a seated desktop without the rule) is made before the session is
   opened. After the desktop's session is active and before hand-over,
   `pkcheck` on the session agent for the installed probe actions (login1
   power-off, reboot, suspend; NetworkManager network-control; udisks2
   filesystem-mount; timedate1 set-time; installed = present in the
   `.policy` files). A seated desktop is admitted only when every answer is a
   refusal; a grant or missing answer refuses it and kills its processes at
   once (`LauncherError::PolkitUnprotected`, Deck reason
   `DECK_REASON_POLKIT_UNPROTECTED`). A seatless desktop is refused only for
   an actual grant. No polkit at all admits. Seats are classified per
   desktop (`desktop_seat(provider, native_seat)`): X11 is `ConsoleSeat`, a
   native Wayland desktop (ADR 0015) `ArcenSeat`, a headless one `Seatless`;
   after launch the stricter of that and logind's recorded seat
   (`DesktopSeat::stricter`) is judged, so a seated session is never
   judged seatless. A start-up console self-test
   warns when the rule fails closed for the console.
5. **polkit < 0.106** has no rules: an Arcen-seat desktop is refused; an X11
   desktop is admitted with a `SECURITY:` error on every sign-in (refusing
   would end X11 service on such hosts).
6. **Compatibility.** No wire change; the refusal travels as a close reason.

## Amendment 17 (2026-11-08): cursor images from every Pier that can read them

Recorded with host cursor images (0.18.0). The record parser and the relay are
a trust boundary.
No wire change: Amendment 11's record, capability and cache are reused as they
are.

1. **Sources.** An image comes from whatever the host can read without
   private API. Wayland keeps Amendment 11 (Mutter cursor metadata through
   capenc). An Xorg desktop reads `XFixesGetCursorImageAndName` on every
   `XFixesCursorNotify` (premultiplied ARGB, `CursorImage::from_premultiplied_argb32`).
   Windows reads `GetCursorInfo` at 40 Hz and, when the handle or its
   visibility changes, the cursor's bitmaps (`GetIconInfo` + `GetDIBits`):
   32-bit colour cursors with alpha as straight BGRA, colour cursors without
   alpha and monochrome cursors through their AND/XOR masks
   (`CursorImage::from_icon_planes`). A screen-inverting pixel, which a
   client cursor cannot draw, becomes opaque black with a one-pixel white
   outline so the classic I-beam stays visible on dark and light content.
   The macOS Pier has no public API that reports another process's cursor
   (`hosts/macos/src/cursor_probe.rs` measures `currentSystemCursor`) and
   keeps named shapes.
2. **One shared source for single-pointer hosts.** X11 and Windows feed
   `arcen_input::cursor::HostCursor`: stable per-desktop ids from the
   content-equal registry (no hash: two different images can never share an
   id), repeat suppression, a hidden state (fully transparent image, a blank
   X name, or a Windows application hide while a mouse is present), the
   named fallback shape (X cursor name, Windows `IDC_*` handle) carried in
   `CursorFacts` and the record's fallback byte, and a bound of 30 *new*
   images per second (only fitting a new image costs CPU and link; a known
   image costs an id). A sample over the bound waits, newest first, and is
   applied when the window allows.
3. **Agent hosts relay.** The Windows and macOS Piers run the session in an
   agent that cannot see the lane. The agent sends every visible record with
   its image inline (`agent_cursor_record`); the broker's `PierLaneRelay`
   routes `CursorUpdate` through `arcen_protocol::cursor::CursorRecordRelay`,
   which keeps the lane's side of the cache contract: newest state only,
   strip an image the Deck holds, renumber for the lane, commit only what
   the lane accepted, and resend the current state (with its image) on a
   reopened lane. It fingerprints each mirrored PNG, so an id reused by a
   restarted agent is inlined again rather than shown as the old image.
   `CursorImageCache` moved from `arcen-input` to `arcen-protocol` (it is
   wire contract); `arcen-input` re-exports it.
4. **Negotiation is unchanged.** A Deck that declared `cursor_image_v1`
   gets images and `CursorModeResultMsg.cursor_images: true` from every
   Pier that can produce them; any other Deck gets `cursor_shape` names as
   before. An X11 Deck without the capability now gets the shape of the X
   cursor name through the same path; a Windows Deck without it gets the
   `IDC_*` names, plus `hidden` when an application hides the pointer.
5. **Presentation.** The Deck draws an image at the size it has in the
   video (points per stream pixel), so a brush circle on the host matches
   the brush on the canvas at any Deck zoom or HiDPI setting. On a display
   with a backing scale above one, the root window's cursor is an `NSCursor`
   whose bitmap is at the backing scale (`clients/macos/src/ui/sharp_cursor.rs`)
   with the same point size and hotspot as winit's one-pixel-per-point
   cursor; winit's cursor stays registered for the view's cursor rect, and
   the next UI pass makes the sharp one current again whenever AppKit
   reapplies that rect. Secondary windows keep winit's cursor (their state
   crosses threads; `NSCursor` does not).

## Amendment 18 (2026-11-09): Windows Grading keeps the desktop's sRGB code values

Recorded with the Windows Grading transfer codes (0.18.0): an additive
capability bit and `TransferCharacteristics::serves_request`.

1. **Defect.** Windows Grading captures WGC FP16 scRGB: DWM has already
   decoded every SDR surface with the IEC 61966-2-1 curve into linear light.
   The shared `ScrgbSdrTransform` re-encoded it with the BT.709 OETF (the GPU
   shader hard-coded it) and tagged `bt709`. The Deck presents every SDR
   transfer on an sRGB layer, as the Linux XShm and macOS `ScreenCaptureKit`
   Grading paths expect (both deliver the desktop's code values), so every
   Windows SDR code moved, by up to 16/255 in the shadows, and the picture
   was darker than the desktop. The Deck's Colour check measured it on the lab
   (mean 10.29/255 against the desktop, 0.18/255 against the desktop's sRGB
   re-encoded as BT.709).
2. **Rule.** A linear-light SDR source is re-encoded with the curve the
   compositor linearised with, so code values survive:
   `arcen_media::video::linear_sdr_desktop_transfer(requested, client)` (since
   renamed `sdr_desktop_transfer`, Amendment 19) serves
   a `bt709` request as `srgb` when the client claims `srgb_transfer`. All
   256 eight-bit codes round-trip through FP16 and the ten-bit matrix
   exactly (`every_eight_bit_srgb_code_survives_scrgb_and_the_srgb_transfer`,
   `srgb_grading_conversion_returns_the_desktop_codes_through_the_matrix`);
   ten-bit codes are `code × 1023/255` within one code of FP16 rounding.
3. **Truth.** The served transfer is stated, not hidden: VUI transfer
   characteristics 13, `active_transfer: "srgb"` in READY and the hello, the
   CoreMedia sRGB transfer on the Deck's format description, and the sRGB
   presentation layer it already used. `TransferCharacteristics::serves_request`
   is the single rule that a served `srgb` honours a requested `bt709`
   (shared BT.709 primaries and D65 white, no re-encoding), so the served
   pipeline stays Grading and neither the Deck's negotiated truth nor its
   Colour check reports a transfer change. The reverse pair is a change.
4. **Compatibility.** `ClientVideoCapabilitiesMsg.srgb_transfer` (auth time,
   because the host decides the transfer before `ServerHello`) and its
   `ClientHelloMsg.supports_srgb_transfer` mirror default to false and are
   omitted when false. A Deck without them keeps the BT.709 re-encoding and
   tag it was built against; a new Deck on an older Windows Pier is served
   `bt709` as before. Administrator variant pins and eight-bit sessions
   (including the OpenH264 fallback) are unchanged: eight-bit capture hands
   over code values and re-encodes nothing. HDR (PQ) never enters the rule.
5. **Other Piers.** Linux XShm RGB10 (`Rgb10Signal::Direct`) and macOS
   `ScreenCaptureKit` (`colorSpaceName = sRGB`) already carry the desktop's
   sRGB code values under a `bt709` tag; they are exact on the Deck and keep
   that tag for now. Tagging them `srgb` under the same capability is a
   follow-up for truthful VUI, not a pixel fix. The Linux PQ-to-SDR paths
   (`Rgb10Signal::PqBt2020ToSdrBt709`, a declared PQ Xorg desktop or a Wayland
   HDR monitor served as Grading) still apply the BT.709 OETF and are a
   follow-up for the Linux Host owner.

## Amendment 19 (2026-11-09): Every Pier states `srgb` for Grading; PQ-to-SDR uses the sRGB curve

Recorded with sRGB on every host (0.19.0). Closes Amendment 18 item 5.
No wire change: the `srgb_transfer` capability of Amendment 18 is reused.

1. **One rule, three Piers.** Desktop applications draw sRGB code values on
   every platform Arcen hosts, and the Deck presents every SDR stream on an
   sRGB layer. `arcen_media::video::sdr_desktop_transfer(requested, client)`
   (renamed from `linear_sdr_desktop_transfer`, because it is no longer only
   about linear-light sources) serves a `bt709` request as `srgb` for a
   client that claims `srgb_transfer`. `serve_sdr_desktop(video, client,
   pinned)` adds the scope every Pier shares: only contracts above eight bits
   that no administrator pinned. `DesktopPlan::serving_sdr_desktop` (Linux)
   and `ResolvedVideoPlan::serving_sdr_desktop` (macOS) apply the same rule to
   those Piers' plan shapes; the Windows Pier calls `serve_sdr_desktop`.
2. **How each source keeps its codes.**
   * Linux depth-30 Xorg: XShm RGB10 goes to the matrix untouched
     (`Rgb10Signal::Direct`, chosen by `conversion_for_output`; no OETF on the
     path). Only the tag changes: VUI 13 from NVENC, `transfer=srgb` to
     capenc, `active_transfer: "srgb"` in READY and the hello.
   * macOS Pier: `ScreenCaptureKit` is told `kCGColorSpaceSRGB` for the
     `xf44` Grading surface, delivers it without colour attachments, and
     VideoToolbox encodes it unconverted. The VideoToolbox
     `TransferFunction` becomes `IEC_sRGB`; a real encode reads H.273 13
     back from the SPS, and an encode/decode round trip keeps every code of a
     1024-code ramp under either tag
     (`srgb_grading_is_tagged_in_the_sps_and_keeps_the_codes`).
   * Windows: unchanged from Amendment 18 (sRGB re-encode of linear light).
   * A Rec.2100 PQ desktop served as Grading (an operator-declared Xorg PQ
     desktop) is converted with the new `Rgb10Signal::PqBt2020ToSdrSrgb`:
     PQ decoded at 203-nit graphics white, BT.2020 mapped to BT.709, then the
     sRGB curve instead of the BT.709 OETF. Every grey of an sRGB desktop
     composed into PQ survives exactly, from half-float and from ten-bit PQ
     codes; colours average 0.04/255 (a dark channel beside a bright one is
     limited by the composition's own PQ precision); the BT.709 OETF moved
     greys by up to 16/255
     (`an_srgb_desktop_composed_into_pq_keeps_its_codes_through_the_srgb_conversion`).
3. **Unchanged.** Older Decks keep `bt709` and the BT.709 OETF; eight-bit
   sessions (the Auto/Speed fast paths and their software fallbacks) keep the
   tag they always had; administrator-pinned variants are served as written;
   HDR (PQ) never enters the rule. **Wayland Grading keeps `bt709`**: a
   Wayland wide epoch encodes the shared capture-admission contract, which
   does not yet carry the client's capability, so the Linux Pier does not
   promise `srgb` there. Threading the capability into `AdmissionRequest` and
   choosing `PqBt2020ToSdrSrgb` for `HalfFloatPq` is the follow-up (closed by
   Amendment 20).
4. **Truth.** `serves_request` (Amendment 18) keeps the served pipeline
   Grading and the Colour check's transfer item passing ("served srgb") on
   Linux and macOS exactly as on Windows. capenc's admission tokens gain
   `rgb10-pq-to-srgb` and `f16-pq-to-srgb` for the new conversion.
5. **Lab evidence (Linux Xorg, NVENC, 1512×950 Grading).** A dev Pier on a
   spare port served a depth-30 desktop showing a known pattern; the host's
   own root code values (`XGetImage`, what XShm reads) were compared with the
   Deck-recorded stream decoded off-line. A 0.18.0 Deck received
   `transfer=srgb` (bitstream `iec61966-2-1`, NVENC VUI SRGB) and a 0.17.0
   Deck `bt709`; both streams decode to the same pixels, the host's codes:
   the ten-bit grey ramp within one ten-bit code (99 % of pixels exact at
   eight bits, mean 0.01/255), against 41 ten-bit codes had an OETF been
   applied. On a declared Rec.2100 PQ desktop holding an sRGB picture
   composed at 203 nits, the 0.18.0 Deck's stream returns the sRGB greys
   (98 % exact at eight bits, mean 0.02/255) while the 0.17.0 Deck's stream
   is the BT.709 OETF (mean 11.8/255 from the desktop's codes), as intended
   for an older Deck. On macOS, a real `ScreenCaptureKit` Grading stream
   reads `bt709/srgb/bt709 full` from its own SPS (end-to-end test).

## Amendment 20 (2026-11-09): Wayland Grading states `srgb` and returns the desktop's codes

Recorded with `rc/wlsrgb` (0.19.0). Closes the Wayland exception of
Amendment 19 item 3. No wire change.

1. **The admission carries the curve.** `AdmissionRequest::sdr_transfer` is
   the transfer the parent resolved for the session (capenc's `transfer=`).
   `admit_grading` reads it only for its SDR output: `srgb` re-encodes PQ
   with the inverse of the curve the compositor decoded its SDR content with
   and keeps a ten-bit SDR source's codes; anything else keeps `bt709` and
   the BT.709 OETF. The linear conversion has only the BT.709 curve and
   keeps `bt709`. `AdmittedCapture::transfer` records the served transfer
   and `epoch()` states it, so the encoder's VUI, READY and the Deck's hello
   agree. `ADMISSION` adds `transfer=<token>` after `transform=` only when it
   differs from the served contract's own, so every other line is
   unchanged; a restated or unknown transfer is refused.
2. **Which inverse is a source fact.** `SourceColour::sdr_content`
   (`SdrContentCurve::Srgb | Gamma22`) says how a PQ or linear composition
   decoded its SDR content. Unknown means the piecewise sRGB curve
   (`Rgb10Signal::PqBt2020ToSdrSrgb`, as for a declared PQ Xorg desktop).
   capenc states `Gamma22` (inferred) on Mutter's `force-hdr` route, and the
   admission then picks `Rgb10Signal::PqBt2020ToSdrGamma22`
   (`rgb10-pq-to-g22`, `f16-pq-to-g22`), stated `srgb` because its codes are
   the desktop's own. Its lookup table is spaced by the square root of the
   light, because a pure power has no linear toe and uniform steps merged
   its darkest codes. capenc's NVENC staging of ten-bit PQ converts with the
   admitted conversion itself (`WideSource::AdmittedPq`), checked against
   the contract, rather than re-deriving it.
3. **The Linux Pier applies the one rule.** `apply_initial_video_request`
   resolves a Wayland fidelity session like an Xorg one:
   `DesktopPlan::serving_sdr_desktop(client.srgb_transfer, pinned)` on the
   PQ composition, so a capable Deck's Grading session asks capenc for
   `transfer=srgb`. HDR keeps PQ, an older Deck `bt709`, a pinned variant its
   pin; the eight-bit Auto/Speed routes never read the field.
4. **Not covered.** An HDR request that capenc's admission downgrades to
   Grading still asked for PQ, so its Grading fallback keeps `bt709`. The
   2.2-power fact is inferred from the route and one compositor's
   measurement (GNOME 51.1); the qualification does not measure it yet.
5. **Lab evidence (Rocky Linux 10, GNOME 51.1 prefix, `force-hdr`, ten-bit
   `xBGR_210LE` DMA-BUF, NVENC, 1800×1130 Grading).** A dev Pier on a spare
   port; a full-screen GTK window of flat eight-bit sRGB patches (256 greys,
   R/G/B ramps, mixed colours) in the served desktop; the Deck-recorded
   stream decoded off-line and compared with the drawn codes. 0.18.0 Deck:
   hello `transfer=srgb`, SPS VUI transfer 13, capenc
   `transform=rgb10-pq-to-g22`; greys mean 0.17/255, max 0.72/255, 248 of
   256 levels decode to their own code, shadows within 0.16/255; colour
   patches mean 0.71/255. The same stream through the piecewise sRGB curve
   (first build of this change) left greys mean 2.0/255 and the shadows up
   to 8.5/255 low (grey 17 decoded as 8.5), which is what identified Mutter's
   2.2-power decode. 0.17.0 Deck: `bt709`, VUI 1, `rgb10-pq-to-sdr`, greys
   mean 10.5/255 and up to 20.6/255 low, as intended for an older Deck.


## Amendment 21 (2026-11-11): host pointer positions for a Deck-drawn cursor

Recorded with host cursor positions.

1. **Why.** In client-drawn (`local`) cursor authority the Deck draws the
   cursor where its own pointing device is. A USB-bridged tablet (Native
   tablet) is driven by the host's own driver and moves only the host
   pointer, so the pen had no visible pointer; host-drawn cursors cannot
   cover it (Xorg depth-30 Grading has no cursor in its pixels) and cost a
   video round trip. Application warps and host-side devices have the same
   gap.
2. **A second binary record on the input/cursor lane.**
   `FrameType::CursorPosition` (`0x51`, `arcen_protocol::cursor_position`,
   36 bytes, host → Deck, `ReliabilityClass::InputLowLatency`): sequence
   (strictly increasing per lane, restarting when the lane reopens), output
   and generation as in `CursorUpdate`, the video `monitor_id` of the stream
   that shows the position (0 for the primary/legacy stream), the hotspot in
   that stream's pixels and the pixel space it was measured in (the Deck
   scales by fraction, so a record measured before a resize never lands on
   another grid), and `on_stream`. One record is the whole state; the newest
   supersedes.
3. **Negotiation.** `AuthResponse.cursor_position_v1` (echoed in
   `ClientHelloMsg`); the host confirms with
   `CursorModeResultMsg.cursor_positions`. All additive and omitted when
   false; `PROTOCOL_VERSION` stays 5. An older Deck never receives the
   record.
4. **Only host-caused motion is reported** (`arcen_input::host_pointer`).
   The Pier's platform code reads its pointer; the shared
   `HostPointerReporter` suppresses every position read within 150 ms of the
   Deck's own pointer input (its echo) and unchanged positions, keeps only the
   newest, sends at most 120 per second, and commits only what the lane
   accepted. The Deck's `PointerAuthority` lets the most recent mover lead:
   a host report takes the cursor unless the Mac pointer moved in the last
   150 ms, and any Mac pointer motion takes it back. The Deck draws the
   host's cursor image (or an arrow) at the reported point above the video,
   hides its system cursor over the viewer meanwhile, and never warps the
   Mac pointer. When the host hides its pointer while a bridged pen
   (Native tablet) drives it — GNOME does so for a tablet tool in
   proximity, and its screencast metadata then carries no sprite — the
   Deck draws a small pen mark instead (`led_pointer_mark`). Where the Mac
   pointer is, and whether it moved, comes from AppKit's global pointer
   location, the active application and the topmost window at that point,
   not from the last window event the UI saw: the session window covers
   the screen under a resting pointer, and a bridged tablet never moves it
   (amended in 0.19.0).
5. **Sources.** Xorg (every preset, including depth-30 XShm Grading): the
   XFixes cursor-watcher thread polls `QueryPointer` (8 ms while moving,
   33 ms at rest; this also catches warps, which generate no input events);
   a committed multi-head Xorg desktop maps each unrotated head to its
   stream. Wayland metadata streams: capenc writes a framed-v2 kind 5 record
   (`CapencCursorPosition`, only when the parent passes `cursor-position`)
   whenever Mutter's cursor metadata moves; single-output desktops only.
6. **Not covered yet.** Multi-output Wayland desktops (connector → video
   monitor mapping), rotated Xorg heads, secondary Deck viewports, relayed
   (session-agent) Piers — the shared relay drops the record — and the
   Windows and macOS Piers.
