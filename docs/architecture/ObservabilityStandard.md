# Observability Standard — unified logging, QoS telemetry, and monitoring integration

**Status: DONE (2026-07-25).** Designed in
the Observability Standard design review and
implemented in
its implementation review.
Target-native Windows execution and Linux operational acceptance remain platform
gates; this status does not claim release or production acceptance. The
implemented architecture record is
[`../architecture/observability.md`](../architecture/observability.md).

**Historical implementation decomposition:** seven shared-first PRs (each independently landable). The pure vocabulary
lands in `arcen-telemetry` first, a new tracing-runtime crate `arcen-observability`
second, additive wire fields in `arcen-protocol` third; then each app (macOS Deck,
Linux Pier, Windows Pier) adopts as a thin adapter, and the final PR ships the
sysadmin-facing monitoring documentation and conformance checklist.
**Escalations:** `arcen-protocol` additive fields → Shared/Architecture.
Identity-in-logs policy (§9) and default support-bundle pseudonymization → Release/Security.

> Scope note: unlike the other docs in this folder, this standard spans **clients as
> well as hosts** — logging is a product-wide platform capability. The template's
> Windows/Linux host sections are extended with a macOS Deck section and a
> forward-conformance section for future apps. Span and every gateway change are
> explicitly outside this seven-PR implementation.
>
> The unchecked acceptance boxes below are retained as design and review
> history. They are not marked as externally accepted while merge/PR assignment
> and the target-native platform gates remain pending.

---

## 1. Context — the M&E sysadmin story

Arcen sells into Media & Entertainment. The people who operate it are studio
sysadmins, and the complaints they receive are always the same shape:

- *"This is slow." "It's lagging." "I can't work."*
- *"Oh, I'm on WiFi — I thought that would be good enough."*
- *"I have 1 GB internet, truly"* — measured reality: 10 Mbps over 5G.

Artists will blame the computer. The product's logging must let a sysadmin **prove
what the user experience actually was**, from either end of the connection:

1. **Who** logged in, **when**, **from where** (IP + network type), for **how long**,
   and **why the session ended** (user quit / network died / host crashed / idle).
2. **What the experience was** over the session timeline: FPS delivered vs. decoded,
   round-trip latency, frame drops, input lag — as a queryable series, not anecdotes.
3. **What the network truly was**: WiFi vs. Ethernet vs. cellular, link rate, signal
   strength, and every mid-session change ("the artist walked to the kitchen").
4. A **three-state default**: any log at a glance says *working* / *something is off* /
   *not working*. The operational profile is a runtime dial for deep dives — never a
   service restart, never a rebuild.
5. **Monitoring integration** (Zabbix, journald consumers, Windows Event Viewer /
   collectors, Elastic/Loki): stable numeric event IDs, machine-parseable output,
   documented recipes.
6. **Zero FPS cost**: logging must never sit on the capture/encode/decode/present or
   input paths. Blocking is banned; silent loss is banned; **counted loss** is the
   only allowed failure mode.

Today each active app solves logging on its own. The macOS client and both hosts
have three diverging stacks. This document defines the single standard and the
migration to it; the dormant gateway is not an implementation target.

---

## 2. Current state — what exists, what diverges, what is missing

### 2.1 Inventory (verified against source, 2026-07-22)

| Capability | macOS Deck | Linux Pier | Windows Pier | shared | gateway (dormant) |
|---|---|---|---|---|---|
| Framework | `tracing` | `tracing` | `tracing` | — | `tracing` |
| Init module | `clients/macos/src/logging/mod.rs` | `hosts/linux/src/logging/mod.rs` | `hosts/windows/src/logging.rs` | — |
| Log file | `~/Library/Logs/Arcen/arcen-client.log` (daily) | `/var/log/arcen/arcen-pier.log` (SIGHUP reopen + logrotate) | `%ProgramData%\Arcen\logs\arcen-pier.log` + per-session `sessions\<sid>.log` | retention contract in `arcen-telemetry` | stdout |
| OS event sink | **none** | journald native (`ARCEN_EVENT_ID`, `ARCEN_FIELD_*`) + `/dev/log` fallback — `hosts/linux/src/eventlog.rs` | Windows Event Log, provider `ArcenPier`, bounded 64-slot non-blocking worker — `hosts/windows/src/eventlog.rs` | `ValidatedLifecycleEvent` | JSON lines |
| Lifecycle events (1000–1404) | **not emitted** | emitted | emitted | vocabulary + schemas in `shared/telemetry/src/lifecycle.rs` | `record_lifecycle_event()` |
| Session-end summary | **none** | `SessionEnd` 1103: reason_class, duration_ms, frames_sent/dropped, audio counters | same vocabulary | `StructuredFields` | — |
| Correlation | generates `session_log_id` (UUID), sends in `AuthResponse` | records as `sid` span field | records as `sid` span field | `CorrelationId` | — |
| Periodic QoS logging | **none** | none logged (2 s HealthPong is liveness only) | `HealthStatsMsg` → client every 2 s; capenc stats at INFO | `HealthStatsMsg` contract | — |
| Client-side timing (decode/display/RTT) | **not measured** | n/a | n/a | fields exist in `HealthStatsMsg` but are never filled | — |
| Network interface truth | LAN/WAN classification of *destination* only (`diagnostics.rs`) | none | none | — | — |
| HID / HoIP logging | **zero** (`clients/macos/src/hid/`) | uhid create/remove not evented | n/a yet | — | — |
| Permission/entitlement state | **invisible** (TCC, sustained-execution) | n/a | n/a | — | — |
| Runtime verbosity reload | UI "Log level" live reload | unified `pier.json` + SIGHUP | SCM control codes 200/201/202 | `VerbosityTier` → `LevelSpec` | env only |
| JSON output | none | none | none | — | **yes** (`init_json_logging`) |
| Hot-path discipline | good (no per-frame logs) | good (frame pump silent) | good (per-AU logs only first 3 + keyframes, DEBUG) | — | n/a |

### 2.2 Target-name divergence (must converge)

| Concept | macOS Deck | Linux Pier | Windows Pier | **Canonical (this standard)** |
|---|---|---|---|---|
| network | `arcen::transport` | `arcen::net` | `arcen::net` | `arcen::net` |
| video pipeline | `arcen::video` | `arcen::media` + `arcen::capenc` | `arcen::capenc` | `arcen::media` (pipeline) + `arcen::capenc` (host encode) |
| peripherals | `arcen::usb` | — | — | `arcen::hid` |
| auth | (in session) | `arcen::auth` | `arcen::auth` | `arcen::auth` |
| health | — | `arcen::health` | (in session) | `arcen::health` |

Platform-only extras stay allowed and documented: `arcen::cppipe`, `arcen::eventlog`
(Windows), `arcen::display` (hosts), `arcen::ui` (clients).

### 2.3 The gap map

1. **macOS Deck is the big gap**: no lifecycle events, no session summary, no QoS
   heartbeat, no client timing measurement, silent HoIP, invisible permissions.
2. **No client-side network truth anywhere** — the single most requested sysadmin
   feature (the WiFi story) does not exist on any platform.
3. **`HealthStatsMsg` round trip is half-wired**: host → client only;
   `decode_time_ms` / `display_time_ms` (`shared/protocol/src/messages.rs`) are dead
   fields; no echo sequence, so no true application-level RTT.
4. **No JSON option** outside the dormant gateway.
5. **No three-state rollup** anywhere; a sysadmin must read raw lines and guess.
6. **No monitoring recipes**: journald fields and Event Log IDs exist on the hosts but
   are undocumented for Zabbix/collector consumption.

---

## 3. Architecture — two-layer shared, thin OS adapters

Follows the `arcen-keel` pattern: a pure brain, a shared runtime, thin adapters.

```
┌───────────────────────────────────────────────────────────────────────┐
│  arcen-telemetry (pure, no I/O, #![forbid(unsafe_code)])  — PR1       │
│  vocabulary: event kinds 1000–1999 · CorrelationId · StructuredFields │
│  names: canonical targets + field keys · QosSample/QosTargets         │
│  HealthState + HealthAssessment + caller-clocked hysteresis           │
│  NetworkSnapshot + pure classifier · OperationalProfile · retention  │
└───────────────▲───────────────────────────────────────────────────────┘
                │ contracts only
┌───────────────┴───────────────────────────────────────────────────────┐
│  arcen-observability (tracing-dependent, OS-free)  — PR2  (NEW crate) │
│  ObservabilityBuilder (one init: text|json, EnvFilter, live reload)   │
│  canonical JSON-lines formatter · ObservabilityHandle                 │
│  BoundedSink<T> (bounded queue + worker + drop counter)               │
│  HeartbeatSampler (drains atomic counters on cadence, emits QoS line) │
│  NetworkTruthProbe trait · lifecycle→tracing bridge                   │
└───▲───────────────▲───────────────▲───────────────────────────────────┘
    │               │               │  thin adapters
┌───┴─────────┐ ┌───┴─────────┐ ┌───┴─────────┐
│ macOS Deck  │ │ Linux Pier  │ │ Windows Pier│
│ file, UI,   │ │ journald,   │ │ Event Log,  │
│ NWPath, TCC │ │ SIGHUP,     │ │ SCM 200-202,│
│ probe       │ │ net probe   │ │ net probe   │
└─────────────┘ └─────────────┘ └─────────────┘
                                                  * os_log sink optional/later
```

**Why a second shared crate instead of extending `arcen-telemetry`:**
`arcen-telemetry`'s no-I/O purity is what makes the vocabulary, redaction, and
retention logic deterministic and golden-testable — it must not grow a `tracing`
dependency. But "contracts only, stack per app" has already failed empirically: three
apps hold three diverging target lists and the client emits no events at all.
Requirement "no one-offs" needs a crate every app links, not a convention.

**Why not put the runtime per-app:** the reload handle, the JSON line shape, the
bounded-sink discipline, and the heartbeat cadence are exactly the things that must be
identical everywhere. One implementation, N thin consumers.

**What stays per-app (and must stay):** OS sink protocols (journald datagrams,
Event Log registration + message tables, future os_log), file paths, signal/SCM/UI
control surfaces, and `NetworkTruthProbe` implementations (NWPathMonitor /
`/sys/class/net` / `GetAdaptersAddresses` are inherently platform FFI).

### 3.1 Performance & memory (shared components)

- `evaluate_health()` operates on a caller-provided fixed-size sample window
  (`&[QosSample]`); no allocation, no clock — the caller supplies timestamps.
- `HeartbeatSampler` reads `AtomicU64`/`AtomicU32` counters registered at
  session start; the hot path only does `fetch_add`/`store` (relaxed ordering) —
  never formats, never locks, never allocates.
- `BoundedSink` is `try_send` on a fixed-capacity channel (default 256 for log
  events, 64 for OS event sinks, matching the proven
  `hosts/windows/src/eventlog.rs` worker); one atomic drop counter per sink.
- JSON formatting happens only on the sink worker thread, never on the caller.
- `NetworkSnapshot` is a small POD; classification is a pure match on
  interface facts.

### 3.2 Invariants

- `arcen-telemetry` remains `#![forbid(unsafe_code)]`, no-I/O, deterministic,
  serde-only dependencies. Event IDs are **append-only forever** (golden test).
- `arcen-observability` is OS-free: `tracing`, `tracing-subscriber`,
  `tracing-appender`, `arcen-telemetry` only. All OS FFI lives in the apps.
- Wire types live in `arcen-protocol` only; all changes additive under protocol v3.
- Lifecycle emission is fire-and-forget: failing to record an event never fails
  the operation it describes (existing Windows discipline, promoted to standard).

### 3.3 Cumulative operational profiles

The operator profile and true record severity are independent. The selected
profile includes every lower-numbered profile:

| Profile | Included records |
|---|---|
| **0 Critical** | Essential process/session/authentication state, effective profile, health transitions, telemetry loss, and one compact proof-of-life `HEALTH_SNAPSHOT` every 60 seconds. A Level-0 record may have `severity=info`. |
| **1 Error** | Level 0 plus warnings, errors, retries, fallbacks, denials, queue pressure, and degraded components. |
| **2 Info** | Level 1 plus normal lifecycle, negotiation, network/device changes, and aggregated QoS. |
| **3 Debug** | Level 2 plus bounded state-machine, protocol-stage, queue, and counter diagnostics; never per-frame, packet, HID report, or input event. |

Production and built-in defaults are Level 0. Current development/lab
deployments are explicitly configured to Level 2; this does not alter the
production default. `trace!` is not an operator profile: it remains only an
explicit `ARCEN_LOG` developer escape and cannot suppress mandatory Level-0
records.

---

## 4. Event vocabulary extensions (append-only; 1000–1404 untouched)

Every new kind gets a full `LifecycleEventDefinition` (category, severity,
required/optional fields) in `shared/telemetry/src/lifecycle.rs`, exactly like the
existing entries, plus golden tests freezing the numeric IDs.

### 1500–1599 · Client session (Deck)

`sid`, `user`, `host`, and `peer_addr` are canonical top-level record context,
never duplicated inside the event-specific `fields` object. Emitters must attach
the applicable identity context separately for authentication, connection, and
session events.

| ID | Stable name | Minimum profile | Required fields | Optional fields |
|---|---|---|---|---|
| 1500 | `CLIENT_START` | 0 | version, os, arch | — |
| 1501 | `CLIENT_STOP` | 0 | uptime_ms | reason_class |
| 1502 | `CLIENT_CONNECT_ATTEMPT` | 2 | transport | — |
| 1503 | `CLIENT_CONNECT_OK` | 0 | tls_version | rtt_ms |
| 1504 | `CLIENT_CONNECT_FAIL` | 0 | reason_class (`dns`\|`tcp`\|`tls`\|`auth`\|`timeout`\|`protocol`) | stage |
| 1505 | `CLIENT_SESSION_END` | 0 | duration_ms, reason_class, frames_decoded, frames_dropped | avg_fps, avg_rtt_ms, worst_health, reconnects |
| 1506 | `CLIENT_RECONNECT` | 1 | attempt, gap_ms | reason_class |

All carry the session `CorrelationId` where a session exists (existing mechanism).
1505 is the client-side mirror of host `SessionEnd` 1103 — same sid, two perspectives.

### 1600–1699 · HID / peripheral (HoIP)

| ID | Stable name | Minimum profile | Required | Optional |
|---|---|---|---|---|
| 1600 | `HID_DEVICE_ATTACHED` | 2 | vendor_id, product_id | brand, firmware, transport |
| 1601 | `HID_DEVICE_DETACHED` | 2 | vendor_id, product_id | brand, firmware, transport |
| 1602 | `HID_PASSTHROUGH_START` | 2 | device_id, vendor_id, product_id | — |
| 1603 | `HID_PASSTHROUGH_END` | 2 | device_id, reports_forwarded | errors |
| 1604 | `HID_PASSTHROUGH_ERROR` | 1 | device_id, reason_class (`open_failed`\|`descriptor_read`\|`inject_failed`\|`wire_error`) | — |

Emitted by the client for attach/forward and by the host for inject failures
(`hosts/linux/src/input/uhid.rs` create/write errors map to 1604).

### 1700–1799 · Network truth

| ID | Stable name | Minimum profile | Required | Optional |
|---|---|---|---|---|
| 1700 | `NETWORK_PATH_ACTIVE` | 2 | interface_kind, scope | link_mbps, ssid, rssi_dbm, mtu |
| 1701 | `NETWORK_PATH_CHANGED` | 2 | old_kind, new_kind | old_mbps, new_mbps, reason_class |
| 1702 | `NETWORK_PATH_LOST` | 1 | interface_kind | — |
| 1703 | `NETWORK_PATH_RESTORED` | 1 | interface_kind, gap_ms | — |

`interface_kind` ∈ `ethernet`|`wifi`|`cellular`|`vpn`|`loopback`|`other`.

### 1800–1899 · Health rollup

| ID | Stable name | Minimum profile | Required | Optional |
|---|---|---|---|---|
| 1800 | `HEALTH_OK` | 0 | previous_state, degraded_duration_ms | — |
| 1801 | `HEALTH_DEGRADED` | 0 | dominant_cause (`fps`\|`rtt`\|`loss`\|`input_latency`\|`heartbeat`), value | threshold |
| 1802 | `HEALTH_CRITICAL` | 0 | dominant_cause, value | threshold |
| 1803 | `HEARTBEAT_LOST` | 0 | missed_intervals | — |
| 1804 | `TELEMETRY_DROPPED` | 0 | sink, dropped_count | — |
| 1805 | `EFFECTIVE_PROFILE` | 0 | profile_level, profile_name, profile_source | — |
| 1806 | `HEALTH_SNAPSHOT` | 0 | overall_state | host_state, client_state, QoS summary fields |

1804 is the "counted loss" escape valve (§7): any sink that dropped since the last
heartbeat reports it here and resets its counter. 1806 is the mandatory compact
60-second proof-of-life even when no state transition occurs.

### 1900–1999 · Permission / entitlement

| ID | Stable name | Minimum profile | Required | Optional |
|---|---|---|---|---|
| 1900 | `PERMISSION_GRANTED` | 2 | permission, platform | — |
| 1901 | `PERMISSION_DENIED` | 1 | permission, platform | — |
| 1902 | `PERMISSION_REVOKED` | 1 | permission, platform | — |
| 1903 | `PERMISSION_PENDING` | 2 | permission, platform | — |

`permission` ∈ `input_monitoring`, `screen_recording`, `accessibility`, `microphone`,
`sustained_execution`, `location_for_ssid` (extensible string, lowercase snake).
Answers "is the entitlement actually in effect?" — today invisible. The macOS TCC
input-monitoring prompt outcome (HoIP prerequisite) becomes an auditable event.

---

## 5. QoS heartbeat and the three-state rollup

### 5.1 The `QosSample` (in `arcen-telemetry`)

Superset of the existing `HealthStatsMsg` metric fields plus the client-side half:

```
QosSample {
    ts_ms, fps_actual, fps_target, bandwidth_mbps,
    frames_sent, frames_dropped,            // host side
    frames_decoded, frames_presented,       // client side
    capture_time_ms, encode_time_ms,        // host side
    decode_time_ms, display_time_ms,        // client side  (finally filled)
    rtt_ms,                                 // measured, not estimated (see 5.4)
    input_latency_ms, input_events,
    heartbeat_misses,
}
```

Every metric is optional. `None` means unavailable; unavailable is never encoded
as zero and never treated as healthy. `HealthAssessment` reports host delivery,
client experience, and their worst available overall state separately.

### 5.2 Cadence

- **Client QoS line: every 5 s** at INFO on `arcen::health` — piggybacks the existing
  HealthPing timer (`clients/macos/src/main.rs`), no new wakeups.
- **Host QoS line: every 10 s** at INFO — aggregates five of the existing internal 2 s
  HealthStats samples. A 9-hour render-review session produces ~3,200 host lines, not
  16,000. Per-2 s detail available at DEBUG.
- No timers are added anywhere; both cadences ride existing loops.

### 5.3 Three-state rollup — `evaluate_health()`

Pure function in `arcen-telemetry`; identical logic on both ends.

| State | Condition (any triggers the worse state) |
|---|---|
| **Ok** ("it's working") | fps ≥ 0.9 × target AND rtt ≤ 60 ms AND drop ratio < 0.5 % AND input_latency ≤ 50 ms |
| **Degraded** ("something is off") | fps 0.7–0.9 × target, or rtt 60–150 ms, or drops 0.5–5 %, or input_latency 50–120 ms |
| **Critical** ("it's not working") | fps < 0.7 × target, or rtt > 150 ms, or drops > 5 %, or input_latency > 120 ms, or ≥ 3 missed heartbeats |

Rationale for defaults (all overridable via `QosTargets` in the standard logging
config): at 24 fps content, 0.9× (≈21.6) is where artists perceive judder; 60 ms RTT
is the ceiling for pen/brush work feeling local; 150 ms objectively breaks
interactive work; 0.5 % drops is visible in playback review.

**Hysteresis:** a state transition must hold for **2 consecutive windows** before
the 1800/1801/1802 event fires. The deterministic tracker uses caller-supplied
monotonic timestamps and never owns a clock. No flapping alerts.

**Both sides compute it.** Host rollup = delivery health (capture/encode/drops).
Client rollup = experienced health (decode/present/RTT). **Disagreement is the
diagnosis**: host `Ok` + client `Degraded` ⇒ the network is the problem — precisely
the "artist on 5G claims 1 GB fibre" case, provable from either log.

### 5.4 Completing the wire round trip (additive, protocol v3)

- Reuse the existing `HealthPingMsg.sequence` / echoed
  `HealthPongMsg.sequence` for true application RTT; do not add a duplicate
  sequence.
- A bounded optional client snapshot carries decode/present/drop counters,
  timing, client health, input timing, and current network facts to the host.
- The host combines that snapshot with its delivery sample so **host logs alone
  contain the end-to-end story**: capture → encode → transmit → decode → display.
- Client measurement: atomic EWMA counters updated inside
  `clients/macos/src/pipeline/` decode-done and present paths (a `store`, never a log
  call), drained by `HeartbeatSampler`.
- No operator profile or remote log-level control crosses the wire. A host at
  Debug can expose received client facts without changing the Deck's profile.

---

## 6. Network truth module

### 6.1 Shared types (pure, `arcen-telemetry`)

```
NetworkSnapshot {
    interface_kind: Ethernet | WiFi | Cellular | Vpn | Loopback | Other,
    link_mbps: Option<u32>,
    ssid: Option<String>,        // WiFi only, where OS permits
    rssi_dbm: Option<i32>,       // WiFi only
    scope: Lan | Wan,            // promotes the existing macOS classifier
    mtu: Option<u32>,
}
```

The LAN/WAN classifier in `clients/macos/src/logging/diagnostics.rs`
(`network_scope()`) moves into the shared crate with its existing unit tests.

### 6.2 Probe trait (`arcen-observability`) and per-app implementations

```
trait NetworkTruthProbe {
    fn snapshot(&self) -> Option<NetworkSnapshot>;
    fn subscribe_changes(&self, tx: …);   // change events → 1701/1702/1703
}
```

| App | Mechanism | Location |
|---|---|---|
| macOS Deck | `NWPathMonitor` (kind, changes) + `SCNetworkInterface`/CoreWLAN FFI (link rate, SSID, RSSI). SSID requires location permission: if denied, log `ssid="unavailable(permission)"` and emit **1901 once** — the gap itself is visible. | `clients/macos/src/netinfo/` (new) |
| Linux Pier | `/sys/class/net/<if>/{type,speed}`, `wireless/` presence, netlink for changes | `hosts/linux/src/netinfo.rs` (new) |
| Windows Pier | `GetAdaptersAddresses` (kind, speed) + `WlanQueryInterface` (SSID/RSSI) | `hosts/windows/src/netinfo.rs` (new) |

### 6.3 Emission

- **1700 `NetworkPathActive` at session start, both ends.** On the client it joins the
  existing connect-diagnostics detached thread (`diagnostics.rs`) — off the UI path.
- **1701 on every mid-session change** (WiFi→Ethernet, AP roam with link-rate change).
- Snapshot embedded in each bounded session-end summary.
- **The client snapshot travels to the host** as additive fields alongside the
  existing client-metadata in the auth/hello exchange, so the sysadmin — who usually
  only has host logs — can read: *"client network: wifi, 54 Mbps, rssi −71 dBm,
  scope wan"*. That line ends the "I have 1 GB internet" conversation.

---

## 7. Hot-path safety rules (normative)

1. **No `tracing` call in per-frame / per-packet / per-input-event loops.** Hot paths
   increment registered atomics; `HeartbeatSampler` turns them into log lines at
   cadence. (Existing good behavior in all three apps becomes a written rule.)
2. **Every OS sink goes through `BoundedSink`**: fixed-capacity queue, `try_send`
   only, dedicated worker, atomic drop counter. Generalizes the proven
   `hosts/windows/src/eventlog.rs` worker (64-slot, never blocks session work).
3. **File writers are non-blocking lossy** (`tracing-appender` non-blocking, lossy) —
   a slow disk must never backpressure a session thread. (This flips the macOS
   client's current synchronous file writer; durability moves to an explicit flush on
   clean shutdown.)
4. **Drops are observable**: any sink with `dropped > 0` since the last heartbeat
   emits `TelemetryDropped` (1804) and resets. Silent loss banned, blocking banned;
   counted loss is the only allowed failure mode.
5. **Lifecycle emission is fire-and-forget** — never fails the described operation.
6. Conformance checklist (§11) carries a hot-path sign-off item; CI adds a grep gate
   for `tracing::` inside modules listed as hot-path (frame pump, decode loop,
   HID report callback, input injection).

---

## 8. Structured output & monitoring integration

### 8.1 Canonical JSON Lines (default for service/file logs)

Packaged service/file output is UTF-8 canonical JSON Lines with no ANSI and one
schema-v1 object per line. Human text is only for interactive console/development
output. Key order, field bounds, enum values, and sorted dynamic `fields` are
frozen by the PR1 golden fixture:

```json
{"schema_version":1,"timestamp":"2026-07-24T16:00:00.000000Z","sequence":42,"profile_level":0,"profile_name":"critical","severity":"info","role":"host","component":"pier","platform":"windows","target":"arcen::session","event_id":1100,"event_name":"SESSION_AUTH_OK","category":"machine_authentication","outcome":"succeeded","sid":"canonical-session-correlation-id","user":"DOMAIN\\artist","host":"pier-01","peer_addr":"192.0.2.10:54000","health_state":"ok","message":"session authentication succeeded","fields":{"auth_method":"password","display_backend":"dxgi"}}
```

- Profile fields identify the record's minimum profile, not the process setting;
  `EFFECTIVE_PROFILE` records the latter and its source.
- Stable monitoring records have numeric `event_id` and uppercase `event_name`;
  ad-hoc diagnostics may omit both and are not monitoring triggers.
- `sid`, `user`, `host`, `peer_addr`, and `health_state` are fixed nullable
  top-level fields. Linux/Windows/macOS differences are values such as
  `platform` or `display_backend`, never alternate schemas.
- True `severity` remains independent from `profile_level`.

### 8.2 OS-native channels (kept, now documented)

- **Linux:** journald native protocol remains primary (`ARCEN_EVENT_ID`,
  `ARCEN_EVENT_NAME`, `ARCEN_CORRELATION_ID`, `ARCEN_FIELD_*` — already correct);
  RFC 3164 `/dev/log` fallback kept. RFC 5424 structured-data is explicitly **out of
  scope v1** (journald + JSON file cover the need); noted as a future Span concern.
- **Windows:** Event Log provider `ArcenPier`, Application channel; message table
  extended for the new IDs so Event Viewer shows readable text.
- **macOS client:** canonical JSON file is primary; an os_log sink is optional
  follow-up, not v1.

### 8.3 Standard logging config (all apps)

The Linux `logging` section in unified `pier.json` becomes the cross-app schema:

```json
{ "level": 0, "retention_days": 30,
  "qos_targets": { "rtt_degraded_ms": 60, "rtt_critical_ms": 150, "...": "..." } }
```

`logging.level` is canonical. For one migration release,
`logging.verbosity` retains its old meaning and maps
`0→level 1`, `1→level 2`, `2→level 3`, `3→level 3`; specifying both is an
error. Retention bounds remain unchanged. Built-in/package policy defaults to
Level 0, while named development/lab deployments are configured to Level 2.

Reload without restart, per platform: SIGHUP (Linux), SCM 200/201/202 (Windows),
UI dial + file-watch (macOS Deck). All three drive the same
`ObservabilityHandle::reload()` contract. `ARCEN_LOG` may refine diagnostic
tracing, including trace, but cannot filter Level-0 structured records.

### 8.4 Monitoring recipes → `docs/operations/monitoring.md` (ships in PR7)

- Vendor-neutral file-tail, JSONPath/regex, journald, and Windows Event Log
  examples keyed on event ID, health state, and severity.
- Zabbix is one example consumer, not a shipped template or schema dependency.
- File/JSON examples use standard JSON tooling; platform documentation changes
  only the protected local log path.
- **Alarm-worthy ID table** — the single page a sysadmin needs to wire alerts.

---

## 9. Identity policy (Release/Security escalation)

**Identifiers yes, secrets never, redact-on-export.**

- Usernames, hostnames, peer addresses, and permitted network identity may appear
  in plaintext only in access-controlled local logs and reviewed native sinks.
  They use schema-approved top-level fields; password/token/secret/credential key
  rejection is unchanged. "Who logged in, when, from where" is the sysadmin's core
  local question.
- **Support bundles pseudonymize identities by default** with a per-bundle keyed
  mapping, preserving correlation while the ephemeral key is discarded. Raw
  identity export requires an explicit protected operator action.
- Malformed canonical lines are excluded with a typed manifest notice, never
  copied raw through a fallback path.
- Any future off-box aggregation (Span) defaults to hashed identities unless the
  operator opts in.

---

## 10. PR decomposition, acceptance criteria, end-to-end tests

Seven shared-first PRs. Each is independently landable and reviewable; each states
what a sysadmin can do after it lands.

### PR1 — `arcen-telemetry` pure extensions

New event kinds 1500–1903 plus `EFFECTIVE_PROFILE` and the 60-second
`HEALTH_SNAPSHOT`; `OperationalProfile`; `QosSample`/validated `QosTargets`;
`HealthState`/`HealthAssessment` + caller-clocked hysteresis; `NetworkSnapshot`
+ classifier; canonical names/value objects; and the pure canonical schema-v1
record with exact JSON fixture. `arcen-session` gains the one-release config
migration contract.

**Acceptance criteria**
- [ ] Separate golden tables freeze existing IDs 1000–1404 and every new ID.
- [ ] Hysteresis unit tests: flapping input around a threshold yields exactly one
      transition per sustained change; 2-window rule verified.
- [ ] `assess_health()` covers all threshold bands and the missed-heartbeat path.
- [ ] Crate remains no-I/O, serde-only deps, `#![forbid(unsafe_code)]`.
- [ ] Existing host consumers compile through the deliberate legacy
      `VerbosityTier` path; old numeric meanings are never silently reinterpreted.

**Sysadmin story:** none visible yet — the vocabulary is published and documented.

### PR2 — new crate `shared/observability` (`arcen-observability`)

Builder, JSON formatter, `ObservabilityHandle`, `BoundedSink`,
`HeartbeatSampler`, `NetworkTruthProbe` trait, and lifecycle→tracing bridge.
No product or gateway adoption occurs in this PR.

**Acceptance criteria**
- [ ] Capture-subscriber tests assert the exact JSON line shape (§8.1) byte-for-byte
      against fixtures.
- [ ] `BoundedSink` test: full queue → `try_send` returns immediately, drop counter
      increments, worker drains, no deadlock under load test.
- [ ] `ObservabilityHandle::reload()` changes filtering live in a test subscriber.
- [ ] Formatter output is byte-identical to the PR1 frozen fixture.
- [ ] Crate has no OS-specific dependencies.

**Sysadmin story:** the shared runtime is ready for product adapters.

### PR3 — `arcen-protocol` additive wire fields *(escalation: Shared/Architecture)*

Extend the existing health exchange with a bounded optional client telemetry
snapshot and client `NetworkSnapshot` metadata. Reuse the existing
`HealthPingMsg.sequence`/`HealthPongMsg.sequence` RTT mechanism. `WIRE.md`
explicitly prohibits remote profile control.

**Acceptance criteria**
- [ ] Serde round-trip tests for all new fields.
- [ ] Compat both directions: old client ↔ new host and new client ↔ old host
      (missing fields default, nothing breaks) — `PROTOCOL_VERSION` stays 3.
- [ ] `WIRE.md` documents every field as additive with defaults.

**Sysadmin story:** none yet; the wire is ready.

### PR4 — macOS Deck adoption (the big one)

Lifecycle emission (1500s/1600s/1700s/1800s/1900s); decode/present atomic counters +
`HeartbeatSampler` QoS line at 5 s; true RTT via the existing ping/pong sequence;
`CLIENT_SESSION_END` 1505 with a bounded summary; `NWPathMonitor` probe + 1700/1701; HID logging throughout
`clients/macos/src/hid/` (1600–1604, plus report counters in the heartbeat — never
per-report logs); TCC permission events 1900s (input-monitoring at HID start,
microphone when used); canonical JSON file output; targets renamed to canonical set
(`arcen::transport`→`arcen::net`, `arcen::video`→`arcen::media`,
`arcen::usb`→`arcen::hid`); file writer switched to non-blocking lossy with
clean-shutdown flush.

**Acceptance criteria**
- [ ] A scripted connect→work→disconnect run produces, in order, with one sid:
      1500 → 1502 → 1503 → 1700 → (QoS lines every 5 s) → 1505 → 1501.
- [ ] The 1505 line alone answers: who, host, when, how long, frames, avg fps/rtt,
      worst health state, why it ended.
- [ ] Toggling WiFi→Ethernet mid-session emits 1701 with old/new kind and rates.
- [ ] Suspending the host stream flips the rollup Ok→Degraded→Critical with correct
      hysteresis timing; restoring emits 1800 with `degraded_duration_ms`.
- [ ] Plugging/unplugging a Wacom emits 1600/1601; an active session adds 1602/1603
      with `reports_forwarded` > 0; TCC denial emits 1901 exactly once.
- [ ] Instruments/profiling shows **zero** log-formatting work on decode/present/HID
      threads during steady state.
- [ ] UI log-level dial still live-reloads; `ARCEN_LOG` still overrides.

**Sysadmin story:** one client log proves *"you were on WiFi at 54 Mbps with 140 ms
RTT; decode and display were healthy; the product was not the problem."*

### PR5 — Linux Pier adoption

Migrate init to `ObservabilityBuilder` (journald/syslog sinks retained, now fed via
`BoundedSink`); host QoS line at 10 s + rollup events 1800s; `/sys/class/net` probe +
1700/1701 for the host side; record client `NetworkSnapshot` from the wire into
session spans and `SESSION_END`; canonical JSON file output; uhid inject failures → 1604;
the unified `pier.json` logging schema extended per §8.3.

**Acceptance criteria**
- [ ] journald carries new event IDs with the existing `ARCEN_*` field convention;
      `journalctl ARCEN_EVENT_ID=1801` filters correctly.
- [ ] SIGHUP reload still reopens the file and now also reloads `qos_targets`.
- [ ] Host `SessionEnd`/QoS lines include the client's network snapshot fields.
- [ ] In the lab (Deck → pier-linux.example.internal): the same sid appears in client and host logs;
      timelines align.
- [ ] A Zabbix (or `journalctl`-simulated) trigger on 1801/1802 fires during an
      induced degradation.

**Sysadmin story:** cross-machine correlation by sid; host log alone shows the
artist's network truth; alerting demo works.

### PR6 — Windows Pier adoption

Same as PR5 via SCM: init through `ObservabilityBuilder`; Event Log message table
extended for all new stable IDs; eventlog worker refactored onto shared `BoundedSink`
(identical drop semantics — regression-tested); per-session agent logs gain QoS
lines + rollup; `GetAdaptersAddresses`/WLAN probe; pier.json logging section extended
per §8.3; SCM 200/201/202 behavior unchanged.

**Acceptance criteria**
- [ ] Event Viewer shows 1801 with a readable rendered message.
- [ ] SCM 200/201/202 reload verified unchanged; new `qos_targets` picked up on 201.
- [ ] `BoundedSink` refactor: existing eventlog worker tests pass unchanged.
- [ ] Broker log and per-session agent log both carry canonical targets and sid.
- [ ] Lab (Deck → pier-windows.example.internal): same-sid correlation, health disagreement visible
      when the network (not the host) is degraded.

**Sysadmin story:** full parity — any host OS, same event IDs, same line shape,
same alerting recipe.

### PR7 — monitoring docs, conformance checklist, support-bundle QoS history

`docs/operations/monitoring.md` (§8.4 vendor-neutral recipes + alarm-ID table);
the conformance checklist (§11) added to this doc's graduated home; support-bundle
export gains default identity pseudonymization and bounded QoS/session summaries
*(escalation: Release/Security)*. No gateway work is included.

**Acceptance criteria**
- [ ] Every recipe executed verbatim against real output from PRs 4–6 (paste-tested,
      not theoretical).
- [ ] Bundle stays within the bounded-manifest contract (entry/size caps).
- [ ] Default pseudonymization maps each identity consistently across one bundle.
- [ ] Alarm-ID table covers every event a monitoring system should page on.

**Sysadmin story:** documented path from zero to a Zabbix dashboard with alerts.

### Cross-host end-to-end lab (closes the feature)

Scripted run in `tests/`: Deck → Pier session with induced network degradation
(traffic shaping). Asserts: (a) identical sid in both logs; (b) event sequence
1503 → 1102 → 1700 → 1801 → 1800 → 1103/1505; (c) client and host summaries
agree on duration and frame counts within tolerance; (d) JSON lines from both ends
parse against one schema fixture; (e) host log contains the client's network
snapshot.

---

## 11. Future-proofing — conformance checklist

### Conformance checklist (every future active product)

- [ ] Initializes logging via `ObservabilityBuilder` (text + JSON, standard config §8.3).
- [ ] Uses canonical targets only; platform extras documented in its ARCHITECTURE.md.
- [ ] `sid` on every session-scoped span/line.
- [ ] Emits the mandatory event set: start/stop, connect/auth outcome, session end
      **with summary**, health transitions, network path, permission states.
- [ ] QoS heartbeat at standard cadence (client 5 s / host 10 s) fed by atomics.
- [ ] Runtime profile reload without process restart, wired to `ObservabilityHandle`.
- [ ] Every OS sink behind `BoundedSink`; drops surface as 1804.
- [ ] No `tracing` calls in hot paths (review sign-off + CI grep gate).
- [ ] Integration recipe added to `docs/operations/monitoring.md`.

### Span aggregation (roadmap only; no work in these seven PRs)

Stable numeric IDs + JSON-lines + sid-as-join-key mean Span can aggregate
multi-session telemetry with **no log-format change** — it ingests the same
`QosSample`/lifecycle shapes it already emits for itself. Reserved names (documented
here so nobody invents alternatives): a `TelemetryRelayMsg` in `arcen-protocol` for
host→gateway session telemetry, and the QUIC `FeedbackSnapshot` → `QosSample` bridge
(`shared/transport/src/quic/feedback.rs` already exposes rtt/loss/congestion — it
feeds the same rollup when QUIC activates).

---

## Key existing code this standard builds on

| What | Where |
|---|---|
| Event vocabulary + schemas (extend) | `shared/telemetry/src/lifecycle.rs` |
| Redaction-safe fields (extend allowed keys) | `shared/telemetry/src/lib.rs` |
| Operational profile/retention contracts | `shared/telemetry/src/log_policy.rs` |
| Bounded non-blocking sink pattern (generalize) | `hosts/windows/src/eventlog.rs` |
| journald native sink (retain, feed via BoundedSink) | `hosts/linux/src/eventlog.rs` |
| HealthPing/HealthStats wire contract (complete) | `shared/protocol/src/messages.rs` |
| LAN/WAN classifier + connect diagnostics (promote) | `clients/macos/src/logging/diagnostics.rs` |
| Runtime reload surfaces (standardize behind `ObservabilityHandle`) | Linux SIGHUP `hosts/linux/src/logging/mod.rs` · Windows SCM `hosts/windows/src/service.rs` · macOS UI `clients/macos/src/logging/mod.rs` |
