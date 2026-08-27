# Arcen Wire Protocol — Contract & Changelog

`arcen-protocol` is the **single source of truth** for the Arcen wire.
It supersedes the legacy Python `common/messages.py`. Every peer — the macOS
client and the native Linux/Windows hosts — depends on this crate so the wire
stays byte-identical across all three.

**Ownership:** the macOS-client/protocol session owns this crate. All wire and
crate-API changes route through that session. Host sessions request changes; the
owner implements, versions, updates this file, and announces the diff.

---

## Versioning

- **`wire::PROTOCOL_VERSION: u16`** — the on-wire handshake version. Bump **only**
  on a breaking binary/JSON change. Adding an *optional* field (with
  `#[serde(default)]`) is backward-compatible and does **not** bump it.
- **`input_protocol_version`** (in the hellos) — input-capability sub-version,
  independent of `PROTOCOL_VERSION`.
- **`audio_output.protocol_version`** (in the hellos) — audio-output
  capability/configuration sub-version, independently fixed at v1.
- **Crate semver** — signals Rust API intent to workspace consumers. Before
  `arcen-protocol` reaches 1.0 we may harden Rust-only constructors, field
  visibility, and `#[non_exhaustive]` coverage without changing the wire or
  bumping `PROTOCOL_VERSION`; those are crate-API breaks, not wire breaks.

Current: `PROTOCOL_VERSION = 3`, `input_protocol_version = 3`, crate `0.1.0`.

The region-scoped input structs below are additive, dormant wire vocabulary.
Products do not advertise or send them yet, so neither active version constant
changes in this tranche. A later product-adapter delivery must add explicit
capability negotiation before enabling them.

---

## Transport framing

Control messages are UTF-8 JSON text frames (each has a `"type"` discriminator).
Media and negotiated clipboard chunks are binary: a fixed big-endian header
followed by the payload.

### Legacy single-monitor video header — 10 bytes, big-endian `!BBBBIH`
| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | `frame_type` | `FrameType` (`0x01` full … `0x04` H265, `0x05` AV1) |
| 1 | 1 | `codec` | `VideoCodec` (`0x00` JPEG … `0x04` AV1) |
| 2 | 1 | `chroma` | `ChromaSubsampling` (`0x00` 420, `0x01` 422, `0x02` 444) |
| 3 | 1 | `flags` | frame flags (e.g. keyframe) |
| 4 | 4 | `timestamp_ms` | `u32` |
| 8 | 2 | `monitor_id` | must be `0` |

### Region video v1 header — 26 bytes, big-endian `!BBBBIHQQ`
| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | `frame_type` | region-only full/H264/H265/AV1 = `0x06..=0x09` |
| 1 | 1 | `codec` | `VideoCodec` |
| 2 | 1 | `chroma` | `ChromaSubsampling` |
| 3 | 1 | `flags` | frame flags (e.g. keyframe) |
| 4 | 4 | `timestamp_ms` | `u32` |
| 8 | 2 | `monitor_id` | nonzero negotiated session monitor id |
| 10 | 8 | `topology_generation` | nonzero applied topology generation |
| 18 | 8 | `stream_epoch` | nonzero epoch for this monitor's current encoder stream |

Region sessions use only this v1 header. A legacy 10-byte video type with a
nonzero monitor id, a short region header, or a zero region generation/epoch is
malformed and rejected before queueing or decoding.

### Audio header — 8 bytes, big-endian `!BBHI`
| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | `frame_type` | always `FrameType::Audio` = `0x10` |
| 1 | 1 | `codec` | `AudioCodec` (`0x00` Opus, `0x01` PCM) |
| 2 | 2 | reserved | zero |
| 4 | 4 | `timestamp_ms` | `u32` |

### JPEG header — 9 bytes, big-endian `!BHHHH` (legacy compat)
`frame_type`, `x`, `y`, `w`, `h`. Size constant `JPEG_HEADER_SIZE = 9`.

Other constants: `PNG_MAGIC`, `PNG_MAX_BYTES = 64 MiB`, `CHUNK_BYTES = 1 MiB`.

### Clipboard v1 header — 20 bytes, big-endian

Clipboard frames are legal only when both peers negotiated exact
`clipboard_protocol_version = 1`. `PROTOCOL_VERSION` remains 3.

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | `frame_type` | `FrameType::Clipboard = 0x20` |
| 1 | 1 | `kind` | `0x00` UTF-8 text, `0x01` PNG |
| 2 | 2 | reserved | must be zero |
| 4 | 8 | `sequence` | nonzero monotonic sender `u64` |
| 12 | 4 | `total_size` | `1..=20 MiB` |
| 16 | 4 | `offset` | next contiguous byte offset |
| 20 | N | payload | `1..=CHUNK_BYTES`; checked end ≤ total |

An accepted `ClipboardDataMsg` precedes its chunks. Receivers retain one
reassembly, accept contiguous metadata-matching chunks only, expire after five
seconds without progress, and replace/scrub an older partial item only for a
newer accepted sequence.

### Hard USB bridge v1 frames — little-endian, reliable stream only

These frames are legal only when both peers advertise `usb_hard_v1` and the
connection requested/accepted `TabletModeMsg::WacomUsbBridge`. They are
normalized URBs, not USB/IP packets, and remain on Arcen's authenticated QUIC
session.

| Type | Value | Fixed bytes | Payload |
| --- | --- | --- | --- |
| `UsbBridgeUrbSubmit` | `0x40` | 33 | OUT data only |
| `UsbBridgeUrbCancel` | `0x41` | 13 | none |
| `UsbBridgeUrbComplete` | `0x42` | 20 | successful IN data only |

Every frame binds a nonzero attachment generation and URB ID. Submit also
carries endpoint, transfer kind (`control|interrupt`), setup-presence flag,
timeout (`1..=1000 ms`), declared length, and an eight-byte setup packet slot.
Completion carries a closed status enum and exact actual length. Metadata is
validated before payload dispatch; stale/zero IDs, unsupported types, nonzero
reserved bytes, mismatched lengths, oversized payloads, and data on failed
completions fail closed.

Control/cancel submissions route as `ReliabilityClass::Control`; completions
route as `InputLowLatency`, which remains `ReliableStream` and is never
datagram-eligible.

---

## Control messages

Each JSON object carries a `type` string. See `messages.rs` for the authoritative
struct definitions and defaults; consts are `messages::{CLIENT_HELLO, …}`.

### Multi-monitor v1 foundation negotiation (protocol v3, additive)

Deck and the Linux/Windows Piers use this contract for negotiated region
sessions. Peers that do not negotiate `multi_monitor_v1` remain on the legacy
primary-only path.

- `AuthRequest.multi_monitor_v1` is an optional pre-auth host offer carrying
  `{max_monitors,supported_rotations,carriers}`. Missing offer means
  "unsupported": a client must keep authentication primary-only and must not
  send a Match My Layout v1 request that a legacy host would silently degrade.
- `AuthResponse.multi_monitor_v1` is optional and now carries an auth-time
  request wrapper `{requested_topology,carriers}` only after the preceding
  `AuthRequest.multi_monitor_v1` explicitly advertised support. The
  `requested_topology` contains the richer Match My Layout roster: bounded
  opaque `client_display_id` strings plus separate legacy `client_monitor_id`
  compatibility values, requested logical layout
  (`x`,`y`,`logical_width`,`logical_height`), requested stream size
  (`width_px`,`height_px`), rotation, physical size, and safe area policy.
  `carriers` is the client's ordered, non-empty supported carrier list. Auth
  construction rejects requests whose carrier list has no intersection with the
  advertised host offer, so the host can choose `selected_carrier` before
  display apply / `ServerHello`. The existing `monitors[]` roster remains the
  legacy primary-only fallback and is still what old peers understand.
- `ClientHelloMsg.device_capabilities["multi_monitor_v1"]` may carry additive
  client capability and diagnostic echo data:
  `{max_monitors,supported_rotations,carriers,requested_topology}`.
  When `requested_topology` is present, the legacy `client_hello.screen_width`
  and `screen_height` must still echo that topology's primary monitor
  `width_px/height_px`, matching `AuthResponse`. This sidecar is not the
  carrier-selection authority; auth-time `{requested_topology,carriers}` is.
- `ServerHelloMsg.device_capabilities["multi_monitor_v1"]` may carry additive
  host capability and applied-topology metadata:
  `{max_monitors,supported_rotations,fixed_topology,topology_backend,carriers,applied_topology}`.
  `applied_topology` holds `{topology_generation,desktop_x,desktop_y,
  desktop_width_px,desktop_height_px,translation_x,translation_y,
  selected_carrier}`. Every `applied_topology.desktop_*`,
  `applied_topology.translation_*`, and mirrored applied-monitor
  `x/y/width_px/height_px` value is in one coherent host-pixel desktop space;
  none of them are derived by mixing the requested logical origin with a
  physical extent.
- `ServerHelloMsg.monitors` keeps its pre-existing legacy JSON schema exactly.
  The richer applied multi-monitor roster stays only in
  `device_capabilities["multi_monitor_v1"].applied_topology.monitors`, whose
  entries are
  `{client_display_id,session_monitor_id,x,y,width_px,height_px,refresh_hz,
  rotation,is_primary,media_plan}` objects, where `media_plan` carries
  `{stream_epoch,encoder_backend,encoder_class,codec,chroma,width_px,height_px,fps,
  bitrate_kbps,cursor_mode,degraded}`. `session_monitor_id` must be in
  `1..=65535`; `0` is reserved for legacy single-monitor framing only.
  `stream_epoch` is nonzero and fences that monitor's decoder state across
  pipeline replacement. `bitrate_kbps` is the host-authoritative per-region
  bitrate budget and must be nonzero; hosts derive it from the single shared
  `arcen-media` `BitrateBudgetKbps` policy and Deck revalidates it through
  that value object's `100..=500_000` kbps band. The wire field's type and
  nonzero invariant are unchanged. Every entry is authoritative independently:
  clients
  must not infer a monitor's codec, chroma, backend, dimensions, or fps from
  the primary/first entry or from the legacy global hello fields. A
  multi-monitor applied entry without `stream_epoch` is rejected.
- `RotationMsg` tokens are `degrees0|degrees90|degrees180|degrees270`;
  `SafeAreaPolicyMsg` is `standard_fullscreen|full_frame`;
  `TopologyBackendKindMsg` is `dedicated_xorg|physical_outputs|virtual_outputs`;
  `MultiMonitorCarrierMsg` is
  `muxed_reliable_stream|per_monitor_reliable_stream`.
- `applied_topology.topology_generation` is nonzero when present. Every region
  video frame repeats that generation plus its roster entry's `stream_epoch`
  in the 26-byte region header. `monitor_id=0` remains exclusive to the legacy
  10-byte single-monitor header.
- Old peers stay primary-only. Missing `AuthRequest.multi_monitor_v1` or
  `device_capabilities["multi_monitor_v1"]` means "unsupported", not "degrade
  silently". A new Deck must therefore omit `AuthResponse.multi_monitor_v1`
  entirely when talking to a legacy host. Unknown additive fields are ignored
  per current JSON compatibility policy.
- Session-global audio remains unchanged; there is no monitor identity in audio
  headers or audio control messages.
- Match My Layout sessions use region-scoped pointer/pen wire vocabulary.
  Legacy single-monitor sessions retain the desktop-coordinate messages.

### Region-scoped input v1 (input protocol v4)

The typed JSON DTOs are `RegionPointerMotionMsg`,
`RegionPointerButtonMsg`, `RegionPointerScrollMsg`,
`RegionPointerEnterMsg`, `RegionPointerLeaveMsg`, and `RegionPenEventMsg`.
Their type discriminators are respectively `region_pointer_motion`,
`region_pointer_button`, `region_pointer_scroll`, `region_pointer_enter`,
`region_pointer_leave`, and `region_pen_event`.

Every message carries:

- nonzero `region_generation: u64`;
- nonzero `region_id: u32`;
- signed `logical_x` / `logical_y` in region-local 1/120-logical-pixel units;
- a nonzero globally increasing `sequence`, `timestamp_ns`, and
  `coalescable`.

Scroll deltas (`delta_x`, `delta_y`) use the same fixed-point logical units.
Button zero is invalid. Pen pressure/tilt/rotation retain the validated ranges
of `PenEventMsg`. Golden JSON vectors live under
`shared/protocol/tests/vectors/`.

The existing normalized `x`/`y` plus mapped `server_x`/`server_y` fields on
`MouseMoveMsg`, `MouseButtonMsg`, `MouseScrollMsg`, `KeyEventMsg`, and
`PenEventMsg` remain byte-compatible for deployed products but are **legacy
desktop-coordinate forms**. They may be used only by non-region
single-monitor sessions. A Match My Layout session requires both peers to
advertise `input_protocol_version >= 4` and
`input_capabilities.region_input = available`; Deck then sends only the
Region* pointer/pen forms, and Pier maps the typed region generation/id and
fixed-point logical position through its committed region adapter. Unknown,
missing, or input-v3 capability fails Match My Layout closed rather than
falling back to `server_x`/`server_y`.

Region input v1 has no relative-motion DTO. Deck therefore disables pointer
lock for Match My Layout and must not send legacy `mouse_move_relative` until
a region-scoped relative-input contract is negotiated.

### Build identity capability

Both hellos may carry a typed
`device_capabilities["build_identity"]` object:

```json
{
  "product": "arcen-deck-macos",
  "version": "0.1.0",
  "build_id": "region-20260809T174248Z",
  "source_revision": "558ead62e6bc+dirty.25e1f2356dd1",
  "build_profile": "release",
  "feature_profile": "quic-default",
  "artifact_sha256": "...",
  "signing_state": "developer-id-release"
}
```

The field is optional for legacy peers. `artifact_sha256`, when present, is the
runtime hash of the exact executable that sent the hello. It is diagnostic
artifact identity, not a trust anchor; TLS identity, platform code signing, and
license verification remain separate security decisions.

### Clipboard subprotocol negotiation (protocol v3, clipboard v1)

- `ClientHelloMsg.clipboard_protocol_version` defaults to zero. The four legacy
  flat clipboard booleans now default false and cannot enable clipboard without
  exact version 1.
- `ServerHelloMsg.clipboard` is optional and carries
  `{protocol_version,direction,content,max_bytes}`. Hosts advertise their
  authoritative policy; disabled/ineligible sessions use Disabled or absence.
- `ClipboardDataMsg` has `type="clipboard_data"`, nonzero `sequence`,
  `kind`, nonzero `size_bytes`, and text-only `truncated`.
- New Deck + old Pier and old Deck + new Pier remain clipboard-disabled. Unknown
  binary-frame tolerance is not assumed; no clipboard frame is sent without
  exact negotiation.

### Audio-output subprotocol negotiation (protocol v3, audio v1)

- `ClientHelloMsg.audio_output` and `ServerHelloMsg.audio_output` default to
  absent. Audio-v1 capabilities are valid only for exact version 1, 48 kHz,
  stereo, 20 ms, at most the two existing codecs, and disabled FEC/DTX.
- `AudioStreamResultMsg` has `type="audio_stream_result"`, an enabled flag,
  optional exact configuration, and a bounded enum reason. The valid bitrate
  tiers are off, 32, 64, 128, 256, and 510 kbps.
- A host sends real Opus only after exact v1 capability selection. A Deck
  accepts real Opus only after an enabled, valid v1 result selecting Opus.
- Missing/mismatched/malformed capabilities fail closed. Legacy peers receive
  byte-compatible PCM and no audio-v1 result.
- The eight-byte binary audio header and `AudioCodec` discriminants remain
  unchanged. The old Opus-tagged raw-PCM workaround is legal only in legacy
  Deck mode and cannot activate the real Opus decoder.

### Client QoS/network telemetry (protocol v3, additive)

This is an additive **health-exchange extension**, not a remote logging
control channel. It exists so that a host, at its own local log profile (see
`docs/architecture/ObservabilityStandard.md`), can show the end-to-end client
experience/network story in its own log — the client's local log profile is
never controlled or overridden by anything in this extension. **No field here
ever selects, raises, lowers, or otherwise controls a peer's local log
level/profile** — every numeric/enum field is validated on untrusted
deserialization (out-of-range and semantically impossible combinations are
rejected, not coerced), and the one field carried here that is itself
sensitive (SSID) is additionally gated by an explicit local disclosure policy
described below — it is never a "non-sensitive facts only" channel.

- **Sequence reuse:** application RTT reuses the existing
  `HealthPingMsg.sequence` echoed by `HealthPongMsg.sequence`. There is no
  second, telemetry-specific sequence field anywhere in this extension.
- `ClientHelloMsg.network_snapshot: Option<ClientNetworkSnapshotMsg>` carries
  the client's *initial* network-path facts. Absent means legacy/unavailable,
  never a healthy/default guess. Subsequent snapshots do **not** get a new
  message type; they ride the existing periodic `HealthPingMsg` (client sends
  every ~5 s) via `client_telemetry.network`, so the host log sees path
  changes over the life of the session.
- `HealthPingMsg.client_telemetry: Option<ClientTelemetrySnapshotMsg>` carries
  the client's bounded QoS sample (`client_telemetry.qos`) and/or its current
  network snapshot (`client_telemetry.network`). Either sub-field, or the
  whole envelope, may be absent independently — absence always means "not
  observed this window", never zero/healthy. `HealthPingMsg` keeps its full
  `Eq`/`PartialEq` contract: every telemetry field added by this extension is
  integer, enum, or a validated bounded newtype, never a float.
- `ClientQosSampleMsg` fields are all `Option`: `frames_received`,
  `frames_decoded`, `frames_presented`, `frames_dropped` (`u64`),
  `decode_time_ms`, `display_time_ms`, `input_send_time_ms` (`u32` whole
  milliseconds — integral by construction, so negative/NaN timings cannot be
  represented), `client_health` (`HealthStateMsg`), `sample_window_secs`
  (`SampleWindowSecs`, a validated `1..=3600` second aggregation window; zero
  and out-of-range values are rejected on deserialization), and
  `sample_age_ms` (`u64`, how stale the sample was when the ping was sent).
- `ClientNetworkSnapshotMsg` mirrors `arcen-telemetry::NetworkSnapshot`'s
  vocabulary and bounds exactly (without `arcen-protocol` depending on
  `arcen-telemetry`) and validates every value on deserialization via a
  private wire shape plus constructor, never trusting raw input:
  - `interface_kind` (`NetworkInterfaceKind` = `ethernet | wifi | cellular |
    vpn | loopback | other`) and `scope` (`NetworkScopeMsg` = `lan | wan`) are
    **mandatory** once a snapshot is present at all — there is no `unknown`
    fallback variant, so a partially populated snapshot is rejected as
    malformed rather than silently defaulted;
  - `link_mbps: Option<u32>` — when present, must be nonzero;
  - `rssi_dbm: Option<i32>` — when present, must be in `-127..=0` dBm;
  - `mtu: Option<u32>` — when present, must be in `576..=65_536` bytes. The
    upper bound intentionally exceeds the 16-bit `65,535` Ethernet/jumbo-frame
    ceiling because real loopback interfaces report a larger, still-bounded
    value: Linux `lo` and current Windows loopback adapters both default to
    exactly `65,536` bytes, and this defensive `u32` range represents that
    truthfully instead of rejecting or silently clamping it;
  - `ssid: Option<NetworkIdentityMsg>` — a bounded, non-empty,
    control-character-free identity string (`1..=MAX_NETWORK_IDENTITY_BYTES =
    64` UTF-8 bytes) safe to place directly into a log line;
  - `ssid`/`rssi_dbm` are only valid when `interface_kind == wifi`; attaching
    either to any other interface kind is rejected
    (`WifiFactsOnOtherInterface`), since those facts are physically impossible
    on a non-Wi-Fi link.
  - **Privacy (SSID is sensitive, disclosure is not implied by read
    permission):** a raw SSID is a sensitive network/location identity — it
    can identify or approximately geolocate the network a device is on. The
    local OS/user permission that lets a client *read* its own SSID is
    **not**, by itself, authorization to *disclose* that SSID to a remote
    host over this wire field. Populating `ssid` on the wire therefore
    additionally requires **explicit local Deck/operator policy** that
    authorizes remote disclosure of network identity for that session/device;
    absent such policy, the client MUST omit `ssid` even though the
    `interface_kind`/`scope`/other facts may still be sent. Default client
    behavior (no explicit disclosure policy configured) is to omit `ssid`.
    Any exported support bundle or shared log that does carry a disclosed
    SSID MUST apply the identity-pseudonymization policy in
    `docs/architecture/ObservabilityStandard.md` (default redaction) before it leaves
    the collecting device. This is a wire-contract requirement frozen now;
    the local policy/consent mechanism itself is platform behavior for a
    later PR.
- `HealthStateMsg` is the shared three-state `ok | degraded | critical`
  vocabulary (mirrors, without depending on, `arcen-telemetry::HealthState`).
  It is only ever carried inside an `Option`; the containing field being
  absent means "not assessed", not `ok`.
- `HealthStatsMsg` gains two purely additive optional fields for **host**
  health metadata that were genuinely missing: `health_state`
  (`Option<HealthStateMsg>`) and `sample_window_secs`
  (`Option<SampleWindowSecs>`, the same validated `1..=3600` second
  aggregation window as above). Every existing field and default is
  unchanged.
- Compatibility is fully additive and `PROTOCOL_VERSION` stays 3:
  - an **old client + new host** sends none of these fields; the host sees
    them as absent (never zero/healthy) and behaves exactly as before;
  - a **new client + old host** may send `network_snapshot`/
    `client_telemetry`, which an old host's JSON decoder silently drops as
    unknown fields (existing "unknown fields are ignorable" policy);
  - a **new host + old client**'s emitted `HealthStatsMsg.health_state`/
    `sample_window_secs` are simply absent fields an old client's decoder
    ignores;
  - no combination of old/new peers ever causes one peer to alter the
    other's local log level, log profile, or sink behavior.
- **Host-side end-to-end visibility, independent of local profile:** the
  point of this extension is that either Pier can surface the *other* side's
  QoS/network facts in its own local log at its own local verbosity — a host
  choosing a more/less verbose local log profile never asks the client to
  change its profile, and a client sending a richer telemetry sample never
  raises what the host chooses to log locally. The wire carries facts only;
  logging policy (what gets written, at what level, and where) is entirely
  local per `docs/architecture/ObservabilityStandard.md` and out of scope here.

### Modeled in Rust today (Tier A)
`quality_settings`, `auth_request`, `auth_response` (+ `monitors[]` of
`ClientMonitor`, `displays_mode`, optional `session_log_id`), `auth_result`,
`client_hello` (optional `session_log_id` consistency echo), `server_hello`
(+ `color_caps`), `health_pong`, `health_stats`, `mouse_move`, `mouse_move_relative`, `mouse_button`,
`key_event`, `broker_machine_request`.

### Tablet mode split (additive, reconnect-scoped)

- `ClientHelloMsg` adds:
  - `tablet_mode_requested: local_termination | wacom_usb_bridge | disabled_mouse_compat`
  - `tablet_mode_capabilities` (`local_termination`, `wacom_usb_bridge`, `disabled_mouse_compat`)
- `ServerHelloMsg` adds `tablet_mode_capabilities` with the same shape.
- Host sends `tablet_mode_result` (`TabletModeResultMsg`) after setup with:
  `requested`, `active`, `accepted`, bounded `reason`, and `reconnect_required`.
- Old peers remain compatible via serde defaults; missing mode fields imply
  `local_termination` request with unknown capabilities. A PR #108-era client
  that proves input-v3 `pen=available` retains local termination even without
  the newer mode-capability field; that compatibility evidence never
  authorizes `wacom_usb_bridge`.
- `wacom_usb_bridge` must never silently fall back and claim success; if a
  bridge backend is unavailable, hosts return `accepted=false` with explicit
  reason and `active=disabled_mouse_compat`. A host never substitutes
  `local_termination`; the user must explicitly select Tablet support.


### Tier-B Stage-1 types now modeled in Rust

These five contracts are additive to protocol v3:

| Rust type | Wire `type` | Fields and Python-compatible defaults |
|---|---|---|
| `RequestFullFrameMsg` | `request_full_frame` | no payload fields |
| `HealthPingMsg` | `health_ping` | `timestamp_ms: u64 = 0`, `sequence: u64 = 0`, `client_state: String = ""` |
| `MouseScrollMsg` | `mouse_scroll` | `x/y/dx/dy: f64 = 0.0`, `server_x/server_y: i32 = -1`, `sequence/timestamp_ns: u64 = 0`, `coalescable: bool = false` |
| `KeyResetModifiersMsg` | `key_reset_modifiers` | `reason: String = "unknown"` |
| `TextCommitMsg` | `text_commit` | `text: String = ""` |

Every struct exposes `msg_type: String` as the JSON `"type"` field, implements
`Default`, and applies serde defaults to every field. `HealthPingMsg` models the
Python dataclass state before `HealthPing.to_json()` stamps the current wall-clock
time; Rust callers that emit a ping remain responsible for assigning the send
timestamp.

> Note: the Rust `auth_response`/`client_hello` already carry honest display
> layout (`monitors[]`, `displays_mode`, real primary `screen_width/height`) that
> the legacy Python dataclasses lacked. This crate is the forward source of truth;
> unknown extra fields were (and are) ignored by older Python peers.

`session_log_id` is a canonical Deck-generated UUID used only for diagnostic
correlation. Authenticated hosts consume it from `auth_response` before creating
session-scoped helpers. Its optional echo in `client_hello` is not authoritative
because helper startup may already have occurred. Missing or invalid values are
replaced at the host edge. These optional fields are additive, so
`PROTOCOL_VERSION` remains 3.

### `key_event` compatibility contract (protocol v3)

- `scan_code` is the legacy field name for a **Qt key identifier**. It is not
  an evdev scan code. Deployed Python hosts pass it directly to XTest or map it
  from Qt to the platform-native code before injection. Changing this field's
  domain requires a future protocol-version migration.
- `modifiers` is the live compact wire mask after client destination policy
  (for example Cmd-to-Ctrl translation) is applied: Shift=`0x01`,
  Ctrl=`0x02`, Alt=`0x04`, Meta=`0x08`, Keypad=`0x10`. These are not Qt's
  high-bit enum values. It defaults to zero when absent so older messages
  remain readable.
- `caps_lock_on`, `num_lock_on`, and `scroll_lock_on` are optional. An omitted
  field means the client cannot observe that lock state; `false` specifically
  means known-off. Hosts must not treat omission as false. Older boolean
  payloads continue to deserialize as known values.

These fields are additive/optional JSON changes, so `PROTOCOL_VERSION` remains 3.

### Cursor authority and relative input (protocol v3, input v2)

Pointer transport and cursor rendering are independent. `PointerMotionMode` is
`absolute` or `relative`; `CursorMode` is `local` or `host`. Cursor authority is
fixed during setup and requires reconnect to change. Pointer lock may switch
motion transport during a connection.

- `AuthResponse.cursor_preference` and
  `ClientHelloMsg.cursor_preference` default to `local` and must agree.
- Both hellos carry typed `input_capabilities` for `absolute_pointer`,
  `relative_pointer`, and `host_cursor`. Missing values are `unknown`.
- New peers advertise `input_protocol_version = 2`. A Deck sends relative input
  only when the server advertises v2 and `relative_pointer = available`.
- `MouseMoveRelativeMsg` has `type = "mouse_move_relative"`, signed `dx/dy`,
  and the existing sequence/timestamp/coalescing metadata.
- `MouseButtonMsg.motion_mode` and `MouseScrollMsg.motion_mode` default to
  `absolute`. In `relative` mode hosts emit the edge/wheel without an absolute
  position first.
- `CursorModeResultMsg` carries requested/active modes, acceptance, and a
  control-free reason bounded to 160 UTF-8 bytes. Deck changes authority only
  after this result.

Sequence zero remains legacy/unsequenced. Every nonzero keyboard, absolute
motion, relative motion, button, and wheel event participates in one strictly
increasing stream. Duplicate or out-of-order events are rejected before native
input state changes.

Compatibility remains additive:

- old Deck to new Pier defaults to absolute motion and local cursor;
- new Deck to old Pier sees no confirmed input-v2 capability, cannot lock, and
  keeps local cursor authority;
- an unavailable host-cursor path returns active `local` with a negative result
  or fails setup with a bounded reason; it never confirms false host authority.

### Negotiated typed pen (protocol v3, input v3)

`input_protocol_version` moved from 2 to 3 to add a negotiated typed-pen
sample. This is purely additive: `wire::PROTOCOL_VERSION` stays 3, and every
new field/message defaults safely on an old peer.

- Both hellos' `input_capabilities` gained six pen fields — `pen`,
  `pen_pressure`, `pen_tilt`, `pen_rotation`, `pen_eraser`, `pen_proximity` —
  each an `InputCapabilityAvailability` (`available` \| `unavailable` \|
  `unknown`, default `unknown`). They mirror `arcen_input::InputCapabilities`'s
  pen truth exactly. As with every other typed capability in this file,
  `unknown` never authorizes the feature: a product may enable typed pen only
  when **both** peers advertise `input_protocol_version >= 3` **and**
  `pen = available` (plus whichever of `pen_pressure`/`pen_tilt`/
  `pen_rotation`/`pen_eraser`/`pen_proximity` it depends on).
- `PenEventMsg` / `PEN_EVENT` (`"pen_event"`) carries one pen sample:
  `x`/`y` (normalized `f64`), `server_x`/`server_y` (`i32`, `-1` sentinel for
  unmapped — same convention as `MouseMoveMsg`/`MouseButtonMsg`/`KeyEventMsg`),
  `pressure` (`f32`), `tilt_x_degrees`/`tilt_y_degrees` (`f32`),
  `rotation_degrees` (`f32`), `tool` (`PenToolMsg`: `tip` \| `eraser`),
  `in_proximity`/`touching` (`bool`), `buttons` (`u16` bitset),
  `sequence`/`timestamp_ns`/`coalescable` (the same low-latency metadata shape
  used by every other input message).
- `PenToolMsg` and `PenEventMsg` mirror `arcen_input::PenTool`/`PenEvent`
  field-for-field so a product's conversion is a straight checked mapping.
  `arcen-protocol` does **not** depend on `arcen-input` (and must not): the
  canonical semantic `PenEvent` stays in `arcen-input`, this crate carries only
  the wire shape, and a product performs the checked conversion at its own
  boundary.
- **Validation is `PenEventMsg::validate()`, not a custom `Deserialize`.**
  `serde` alone accepts a non-finite or out-of-range payload (JSON has no
  NaN/Infinity, but an untrusted peer can still send an out-of-range finite
  value, e.g. `pressure: 2.0`); a product **must** call `validate()` and
  reject the message before converting it, advancing its input sequence, or
  injecting native input. Checked ranges: `x`/`y` inclusive `0.0..=1.0`,
  `pressure` inclusive `0.0..=1.0`, `tilt_x_degrees`/`tilt_y_degrees` inclusive
  `-90.0..=90.0`, `rotation_degrees` inclusive `0.0..=360.0`. Rotation's `0`
  and `360` both denote the same physical angle, so both bounds are accepted
  regardless of a sender's convention.
- `sequence` participates in the same one globally ordered input stream as
  keyboard, absolute motion, relative motion, button, and wheel events;
  sequence zero remains legacy/unsequenced and a rejected/out-of-order
  sequence must not advance product state.

Compatibility remains additive:

- an input-v2 peer (or older) never advertises the six pen capability keys —
  they default to `unknown` — so it is never authorized to send or receive
  `PEN_EVENT`, and it never sees an `input_protocol_version` above 2;
- old Deck to new Pier / new Deck to old Pier: neither side confirms pen
  capability, so both keep pre-existing mouse-only behavior — no
  `pen_event` is ever sent;
- adding the pen fields does not change any existing `MouseMoveMsg`,
  `MouseButtonMsg`, `MouseScrollMsg`, `KeyEventMsg`, or `InputCapabilitiesMsg`
  pointer/cursor field's legacy default.

### Legacy mouse/key input compatibility (protocol v3)

Pre-Phase-3 clients send only the original fields shown below. Rust consumers
must apply the Python dataclass defaults when the later input metadata is absent:

| Rust type | Legacy required fields | Defaults for absent additive fields |
|---|---|---|
| `MouseMoveMsg` | `type`, `x`, `y` | `server_x/server_y = -1`, `sequence/timestamp_ns = 0`, `coalescable = true` |
| `MouseButtonMsg` | `type`, `x`, `y`, `button`, `pressed` | `server_x/server_y = -1`, `sequence/timestamp_ns = 0`, `coalescable = false` |
| `KeyEventMsg` | `type`, `scan_code`, `pressed` | `modifiers = 0`, lock states omitted/unknown (`None`), `server_x/server_y = -1`, `sequence/timestamp_ns = 0`, `coalescable = false` |

The `-1` coordinates preserve normalized-coordinate fallback. Missing key lock
states remain unknown rather than known-off; serialization continues to omit
those `None` fields. These are deserialization fixes for already-additive fields,
so `PROTOCOL_VERSION` remains 3.

### Experimental raw-HID passthrough (quarantined, `experimental_raw_hid`)

The binary `HidDeviceAdded`/`HidDeviceRemoved`/`HidReport` frames (see the
`HID passthrough (HoIP)` layout comment in `wire.rs`) carry raw IOHID input
reports for a small allow-listed set of drawing-tablet vendors (Wacom, Huion,
XP-Pen, UC-Logic, Gaomon) from Arcen Deck to a host's `/dev/uhid`. This path
is **quarantined** and is not part of default builds or the default wire
contract:

- Both `ClientHelloMsg` and `ServerHelloMsg` carry an additive,
  `#[serde(default)] experimental_raw_hid: bool` field (default `false`).
  This is independent of `INPUT_PROTOCOL_VERSION`/typed-pen negotiation above
  and does not gate or is gated by it.
- Actually admitting/sending frames additionally requires each peer's own
  compile-time `experimental-raw-hid` Cargo feature (default off) and an
  explicit runtime opt-in (env var), in addition to both peers advertising
  `experimental_raw_hid = true`.
- `decode_hid_device_added`/`decode_hid_report` reject any claimed descriptor
  or report length above `MAX_HID_DESCRIPTOR_LEN`/`MAX_HID_REPORT_LEN` (4096
  bytes each) unconditionally, regardless of negotiation state, so a hostile
  or buggy peer can never hand an oversize payload to a host's kernel-facing
  `/dev/uhid` parser.
- Default builds and any peer that predates this field (`experimental_raw_hid`
  absent → `false` via `#[serde(default)]`) never send or accept raw HID.

### In legacy Python, not yet in Rust (Tier B — ported on host demand)
`monitor_list` (+ `MonitorInfo`), `select_monitor`,
`resize_request`, `session_configure`, `cursor_update`, `device_capabilities`,
`device_policy`, `input_device_add`/`input_device_remove`, `hid_report`,
`device_feedback`, `device_error`, `error` (`ProtocolErrorMsg`),
`connection_profile`.

Each Tier-B type is ported verbatim from its Python dataclass (field names +
`#[serde(default)]` semantics) with a round-trip test before any host relies on it.
The legacy `resize_request` is superseded by the acknowledged `display_update`
subprotocol below and will not be ported. The legacy `pen_event`/`pen_proximity`
pair is superseded by the single negotiated `PenEventMsg`/`PEN_EVENT` above,
which folds proximity into one typed pen sample rather than a separate message.

### Mid-session stream resize (protocol v3, additive — `display_update`)

Dynamic display fit: the client retargets the single active stream surface to
match its actual viewer size (window resize, fullscreen, HiDPI toggle). Two new
message types and one capability flag; no `PROTOCOL_VERSION` bump.

| Message | Field | Contract |
|---|---|---|
| `ServerHello` | `supports_display_update: bool = false` | Host accepts `display_update` for this session. Absent on old hosts (parses false). Hosts advertise true only when the session actually holds display control. Clients must never send `display_update` unless this was true. |
| `DisplayUpdate` (client→host) | `sequence: u64` | Client-monotonic starting at 1. The host ignores requests whose sequence is not greater than the last applied, so a stale in-flight resize never overrides a newer one. |
| | `width`, `height: u32` | Requested stream size in pixels. Must be even and within 320–16384 × 240–8640 (`MIN_STREAM_*`/`MAX_STREAM_*` in `messages.rs`); the client pre-clamps, the host re-validates. |
| | `scale: f32` | Backing scale the client applied (1.0 logical, 2.0 HiDPI). Diagnostic only. |
| | `reason: String` | `"connect_fit"` \| `"fullscreen"` \| `"resize"` \| `"retina_toggle"`. Logging only. |
| `DisplayUpdateResult` (host→client) | `sequence`, `accepted` | Answers every request with its sequence. |
| | `width`, `height` | The size actually streaming after processing — the new size when accepted, the unchanged current size when rejected. |
| | `message: String` | Bounded human-readable rejection reason; empty on success. |

Rules: `display_update` is legal only after `client_hello`. The host applies at
most one resize per second (later requests coalesce, latest wins). On accept the
host restarts its encoder at the new size and the next access unit carries fresh
parameter sets + IDR — the client decoder rebuilds from the SPS change; the
video framing itself is unchanged. Hosts that predate the message ignore the
unknown type (existing dispatcher contract), which the capability flag makes
unreachable anyway.

### Direct-session resume (protocol v3, additive)

Session resume adds optional/defaulted fields to the existing authentication
messages. It does not add a message type or change `PROTOCOL_VERSION`, which
remains 3. The current product carries these messages over direct QUIC.

| Message | Field/value | Implemented contract |
|---|---|---|
| `AuthRequest` | `auth_methods[] = "resume"` | Advertises that this direct Pier permits resume. Absence means unsupported. During a held detached session, the request still advertises `resume` but omits the already-bound disclaimer. |
| `AuthResponse` | `resume_requested: bool = false` | An initial authenticated connection opts in only when the host advertised `resume`. |
| `AuthResponse` | `resume_holder_nonce: Option<String>` | Initial opt-in and resume attempts carry the same Deck-generated 32-byte nonce as 64 lowercase hexadecimal characters. |
| `AuthResponse` | `resume_grant: Option<String>` | A resume attempt presents the opaque host grant here; an initial request does not. |
| `AuthResponse` | `method = "resume"` | Selects credential-free resume. `username` and `credential` (the password field for PAM/password methods) must both be empty, and `resume_requested` must be false. Display topology, time zone, holder nonce, grant, and the new attempt's `session_log_id` remain present for binding and diagnostics. |
| `AuthResult` | `resume_grant: Option<String>` | On initial opt-in, carries generation 1. On successful resume, carries the already-rotated successor. While an opted-in attachment is streaming, the host also reuses `AuthResult` as an in-band one-slot grant refresh. |
| `AuthResult` | `resume_window_secs: Option<u32>` | Accompanies an issued grant and reports the host-selected post-loss reconnect window `W` (`1..=7200`). The signed claim lifetime is bounded to `2W`; Deck does not start its `W` deadline until transport loss. |
| `AuthResult` | `resumed: bool = false` | True only when the connection attached to the existing native session. |
| `AuthResult` | `error_code: Option<ResumeErrorCode>` | Machine-readable resume failure. Successful results omit it. |

An in-band refresh has the exact shape `type="auth_result"`, `success=true`, a
bounded `message`, nonempty `resume_grant`, `resume_window_secs` in
`1..=7200`, `resumed=false`, and no `error_code`. It is a streaming control
update, not another authentication pause, and Deck sends no
`AcceptAuthentication` acknowledgment. A failure result or malformed refresh
shape during streaming is a terminal protocol error. Legacy or non-opted-in
attachments are never sent a refresh.

The host rotates before sending a refresh or resume-success result. A known
successor serialization/send failure enters final drain immediately. If Deck
later presents the authenticated, unexpired exact predecessor while the slot is detached, the host returns
`replayed` and enters the same drain; it does not accept the predecessor or
return the undisclosed successor. Once cleanup removes the held session, a
subsequent connection may use ordinary visible authentication. Other invalid or
stale candidates do not gain authority to drain the held session.

The stable `error_code` wire strings are:

| Value | Meaning |
|---|---|
| `unsupported` | Direct resume is unavailable or a normal auth attempt arrived while a detached slot requires resume. |
| `expired` | The grant or authoritative reconnect deadline expired. |
| `replayed` | The token was malformed, invalid, stale, already consumed, or did not match the current generation/nonce/holder. |
| `native_identity_changed` | The bound SID/WTS session or uid/logind session changed. |
| `topology_changed` | The TLS SPKI host identity or bound client/display topology changed. |
| `session_gone` | The bound active host session no longer exists. |
| `internal_failure` | The host failed closed without exposing internal detail. |

All additions use serde defaults and are omitted when absent/false. Therefore:

- new Deck + old Pier: no `resume` advertisement, no opt-in, and ordinary
  protocol-v3 authentication continues;
- old Deck + new Pier: unknown fields/advertisement are ignored, no holder nonce
  is sent, no grant is issued, and ordinary authentication continues;
- new peers with reconnect disabled (`resume_window_secs = 0` host policy): no
  advertisement or grant is emitted;
- a detached resumable session accepts only `method = "resume"`; invalid resume
  never falls through to PAM, password authentication, or Credential Provider.

`resume_grant`, `resume_holder_nonce`, credentials, passwords, and HMAC key
material are secrets. Debug output redacts the grant, holder nonce, and
credential; logs and support bundles must not contain them. See
[`../../docs/architecture/session-auto-reconnect.md`](../../docs/architecture/session-auto-reconnect.md).

---

## Auth

- `auth::hash_password(password, challenge)` = `sha256(f"{password}:{challenge}")`
  hex (64 chars). Golden-locked in tests.
- `auth::generate_challenge()` = 64 hex chars from 32 OS-random bytes
  (`secrets.token_hex(32)` equivalent). Host-issued.

---

## Session state machine (`fsm`)

Client states, server states, the client transition table, and the allowed
client/server state pairs (`ALLOWED_PAIRS`, `is_state_pair_allowed`) mirror the
legacy `common/session_fsm.py` contract.

---

## Changelog

- **2026-08-09 — input-v4 region-input-v1 cutover** — Bump
  `input_protocol_version` 3 -> 4 (`wire::PROTOCOL_VERSION` remains 3) and add
  additive `InputCapabilitiesMsg.region_input`. Match My Layout requires both
  peers to prove v4 plus `region_input=available`; Deck serializes
  `region_pointer_enter|leave|motion|button|scroll` and `region_pen_event`
  directly, with no `server_x`/`server_y`. Linux and Windows Pier consume
  those commands through their shared region-state/coordinate adapters and
  reject capability downgrade. The temporary desktop-coordinate adapter is
  limited to non-region single-monitor compatibility sessions.
- **2026-07-28 — protocol v3 additive input-v3 negotiated typed pen** — Bump
  `input_protocol_version` 2 -> 3 (`wire::PROTOCOL_VERSION` unchanged at 3).
  Add six additive `#[serde(default)]` pen fields to `InputCapabilitiesMsg`
  (`pen`, `pen_pressure`, `pen_tilt`, `pen_rotation`, `pen_eraser`,
  `pen_proximity`), mirroring `arcen_input::InputCapabilities`'s pen truth;
  `unknown` never authorizes typed pen. Add `PenToolMsg` (`tip` \| `eraser`)
  and `PenEventMsg`/`PEN_EVENT` (`"pen_event"`) carrying normalized `x`/`y`,
  `-1`-sentinel `server_x`/`server_y`, `pressure`, `tilt_x_degrees`/
  `tilt_y_degrees`, `rotation_degrees`, `tool`, `in_proximity`/`touching`, a
  `buttons` bitset, and the shared `sequence`/`timestamp_ns`/`coalescable`
  metadata. Field ranges are enforced by `PenEventMsg::validate()` (not a
  custom `Deserialize`); a product must call it before converting, advancing
  its input sequence, or injecting native input. Keep the wire DTOs in
  `arcen-protocol` and the canonical `arcen_input::PenEvent` in `arcen-input`
  with no new protocol-to-input crate dependency; product adapters perform
  the checked conversion. Supersedes the deferred legacy `pen_event`/
  `pen_proximity` Tier-B pair. Add golden/round-trip, minimal-JSON-default,
  malformed/out-of-range-`validate()`, and input-v2-peer-never-authorized
  tests.
- **2026-07-25 — protocol v3 client QoS/network telemetry final review
  fixes** — Raise `mtu`'s upper bound from `65,535` to `65,536` bytes: Linux
  `lo` and current Windows loopback adapters both truthfully report exactly
  `65,536`, one past the 16-bit Ethernet/jumbo-frame ceiling, so the
  defensive `u32` range must include it rather than reject/clamp it. Correct
  the contradictory "only non-sensitive facts cross the wire" wording:
  freeze the requirement that raw SSID is a sensitive network/location
  identity whose local OS read permission is not, by itself, authorization to
  disclose it to a remote host — the wire `ssid` field may only be populated
  under explicit local Deck/operator disclosure policy, defaults to omitted,
  and disclosed values in support exports must still be pseudonymized. No
  platform/policy behavior implemented yet; this only freezes the wire-level
  requirement.
- **2026-07-25 — protocol v3 additive client QoS/network telemetry review
  fixes** — Replace floats with `Eq`-safe integer/enum wire types across
  `ClientQosSampleMsg`/`ClientNetworkSnapshotMsg` and restore
  `HealthPingMsg`'s full `Eq` derive. Validate all client QoS/network facts on
  untrusted deserialization via a private wire shape + constructor: reject
  zero link rate, out-of-range MTU (`576..=65_535`)/RSSI (`-127..=0` dBm),
  Wi-Fi-only facts on non-Wi-Fi interfaces, empty/oversized/control-character
  SSIDs, and zero/out-of-range `SampleWindowSecs` (`1..=3600` s, shared by
  `ClientQosSampleMsg` and `HealthStatsMsg`). Align `NetworkInterfaceKind`
  (`ethernet | wifi | cellular | vpn | loopback | other`) and `NetworkScopeMsg`
  (`lan | wan`) exactly with `arcen-telemetry::NetworkSnapshot`'s vocabulary
  (added `loopback`, renamed `internet` to `wan`, dropped the `unknown`
  fallback variants and the `status` field). Correct SSID privacy language:
  it is a sensitive network/location identity requiring applicable
  permission/consent and default export redaction, not a cosmetic label.
- **2026-07-24 — protocol v3 additive client QoS/network telemetry** — Add
  `ClientHelloMsg.network_snapshot`, `HealthPingMsg.client_telemetry`
  (`ClientTelemetrySnapshotMsg` = `ClientQosSampleMsg` + `ClientNetworkSnapshotMsg`),
  and `HealthStatsMsg.health_state`/`sample_window_secs`. Reuse the existing
  `HealthPingMsg.sequence`/`HealthPongMsg.sequence` pair for RTT — no new
  sequence mechanism. Bounded `NetworkLabel` (64 UTF-8 bytes, no control
  characters) covers SSID/status; every new field is optional and absent means
  unavailable/legacy, never zero/healthy. Carries no log-level, log-profile, or
  remote logging-control field. No protocol-version bump.
- **2026-07-22 — protocol v3 additive mid-session stream resize** — Add
  `display_update` / `display_update_result` message pair with client-monotonic
  sequencing, shared `MIN_STREAM_*`/`MAX_STREAM_*` bounds, and the defaulted
  `supports_display_update` capability in `server_hello`. Supersedes the
  deferred legacy `resize_request`. No protocol-version bump.
- **2026-07-21 — protocol v3 additive input v2 cursor modes** — Add typed
  absolute/relative/host-cursor capabilities, defaulted local cursor preference
  in auth/client hello, relative motion, relative edge/wheel semantics, and a
  bounded cursor result. Keep one global input sequence and protocol version 3.
- **2026-07-21 — protocol v3 additive direct-session resume** — Advertise
  `auth_methods[] = "resume"`; add defaulted/optional `AuthResponse`
  opt-in/grant fields and `AuthResult` grant/window/resumed/error fields; lock
  the seven `ResumeErrorCode` wire strings; reuse successful `AuthResult`
  in-stream for one-slot grant refresh without an acknowledgment; add legacy
  JSON, secret-redaction, and result-shape coverage. No protocol-version bump.
- **2026-07-11 — 0.1.3** — Restore backward-compatible deserialization for
  legacy `mouse_move`, `mouse_button`, and `key_event` JSON by applying the
  exact Python defaults to absent coordinate, sequence, timestamp, and
  coalescing metadata. Preserve omitted key lock states as unknown `Option`
  values. Add golden legacy JSON tests. No protocol-version bump.
- **2026-07-10 — 0.1.2** — Tier-B Stage-1 extraction: add
  `RequestFullFrameMsg`, `HealthPingMsg`, `MouseScrollMsg`,
  `KeyResetModifiersMsg`, and `TextCommitMsg` with public wire-type constants,
  exact Python-compatible field defaults, legacy/minimal JSON parsing, and
  round-trip coverage. Additive JSON/API only; protocol version remains 3.
  `ResizeRequestMsg` is deferred to Tier-B Stage 2.
- **2026-07-10 — 0.1.1** — Lock protocol-v3 `key_event` compatibility:
  document legacy `scan_code` as a Qt key identifier, add the defaulted live
  compact-wire `modifiers` mask, and represent unobservable lock states as
  omitted/unknown values instead of false claims. Additive JSON change; no
  protocol-version bump.
- **2026-07-10 — 0.1.0** — Initial extraction from `client/rust/src/protocol/*`.
  Tier-A messages, binary headers, FSM, and `auth` lifted verbatim (wire-verified
  against the Linux host, both chroma modes). `PROTOCOL_VERSION = 3`.
