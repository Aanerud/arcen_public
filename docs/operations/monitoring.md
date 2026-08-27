# Monitoring Arcen Pier and Deck

Audience: sysadmins and SRE/monitoring teams operating Arcen hosts (Pier) and
clients (Deck) who do not have access to, and do not need, the Rust source.
Every path, command, field name, and event ID below is taken directly from the
shipped code and packaging as of this writing; none of it is aspirational.

This guide covers: operational log levels, the canonical JSON Lines (JSONL)
schema, the stable event-ID vocabulary, where records land on each platform
(including native OS log mirrors), reusable monitoring recipes, host-side
end-to-end diagnosis, and incident workflows. It intentionally does not
duplicate:

- Certificate lifecycle and rotation mechanics — see
  [`tls-certificates.md`](tls-certificates.md).
- Support-bundle collection, redaction, and privacy guarantees — see
  [`support-bundles.md`](support-bundles.md).
- Trust boundaries between Pier, Deck, and the network — see
  [`../security/trust-boundaries.md`](../security/trust-boundaries.md).
- The full observability design rationale and PR decomposition — see the
  approved plan, [`../architecture/ObservabilityStandard.md`](../architecture/ObservabilityStandard.md)
  (design reference; this document describes what has actually shipped).

## 1. Operational profiles and severity

Arcen separates **how much is logged** (the operational profile) from **how
bad an individual event is** (its severity). They are independent axes.

### 1.1 The four cumulative profiles

| Level | Name       | Meaning                                            |
| ----- | ---------- | --------------------------------------------------- |
| 0     | Critical   | Only events an operator must see to know the service is broken. |
| 1     | Error      | Level 0 plus events worth investigating soon.      |
| 2     | Info       | Level 1 plus routine lifecycle audit trail.        |
| 3     | Debug      | Level 2 plus verbose diagnostics for active troubleshooting. |

Profiles are **cumulative**: raising the level never hides a lower-level
event, it only adds more. A process running at Level 2 (Info) still emits
every Level 0 and Level 1 event.

- **Production default: Level 0 (Critical).** Packaged configuration ships
  `"level": 0` explicitly (`packaging/linux/arcen-pier.json`,
  `packaging/windows/pier.json`).
- **Development/test default: Level 2 (Info).** Development deployment
  configuration overrides raise the profile to 2 (see
  `hosts/windows/INSTALL.md`).
- A legacy `logging.verbosity` (Quiet/Normal/Debug/Trace, 0-3) setting still
  works and is migrated to the new profile as Quiet→Error(1),
  Normal→Info(2), Debug→Debug(3), Trace→Debug(3). `logging.level` and
  `logging.verbosity` are mutually exclusive in one config file — setting
  both is a configuration error.

### 1.2 Reload surfaces (no restart required)

| Platform | Command | Effect |
| -------- | ------- | ------ |
| Linux (Pier) | `sudo systemctl reload arcen-pier` | Sends `SIGHUP`, which reopens the log file and re-reads the configured profile/QoS thresholds. |
| Windows (Pier) | `sc.exe control ArcenPier 201` | Reloads the configured profile and QoS thresholds from `pier.json`. |
| Windows (Pier), temporary override | `sc.exe control ArcenPier 200` | Switches to Level 3 (Debug) **temporarily**, for active troubleshooting, without editing the config file. |
| Windows (Pier), TLS only | `sc.exe control ArcenPier 202` | Reloads the TLS certificate only — it does **not** change the log level. |
| macOS (Deck) | "Log level" control in the Deck UI | Applies immediately via the in-process handle; there is **no file-watch/hot-reload of a config file** on macOS — the UI control is the only reload surface. |

All three surfaces ultimately call the same in-process
`ObservabilityHandle::reload_profile()` (or `reload_profile_with()` for a
temporary override), so the effect is identical regardless of which
platform-specific trigger you use.

Every successful profile change (including at startup) emits an
`EFFECTIVE_PROFILE` record (event ID 1805, see §3) with fields
`profile_level`, `profile_name`, and `profile_source`. Observed
`profile_source` values include `production_default`, `config_level`,
`config_legacy_verbosity`, `cli_override` (Linux), and `startup`,
`user_setting` (macOS) — use this event to confirm what level a process is
actually running at, rather than assuming the packaged default is still in
effect.

**There is no remote log-level control.** A Deck cannot change a Pier's
profile, and a Pier cannot change a Deck's profile. Each process's profile is
controlled only by its own local config file, CLI flag, or (Windows/macOS)
local reload surface above.

### 1.3 Proof-of-life vs. QoS cadence

| Signal | Cadence | Purpose |
| ------ | ------- | ------- |
| `HEALTH_SNAPSHOT` (event 1806) | 60 seconds, on every platform, at every profile including Level 0 | Mandatory proof-of-life: if you have not seen a `HEALTH_SNAPSHOT` from a process in over ~2 minutes, treat the process as unresponsive even if nothing else in the log says so. |
| Deck→Pier QoS sample (`HealthPingMsg.client_telemetry`) | 5 seconds | Client-side frame/RTT/drop sampling, sent to the host over the existing session transport. |
| Pier aggregate health rollup | 10 seconds | Host-side aggregated health assessment (`HealthState`) computed from the last window of samples. |
| Pier internal detail samples | 2 seconds | Finer-grained internal sampling window feeding the 10-second rollup; only interesting at Debug. |

The three-state health rollup (`ok` / `degraded` / `critical`) uses two
consecutive windows of unhealthy samples before declaring `degraded` or
`critical` (hysteresis), and two consecutive healthy windows before
declaring `ok` again — a single bad sample does not flip the state, and a
single good sample does not clear a degraded/critical state either. Default
QoS thresholds: frame delivery below 90%/70% of target, RTT above
60ms/150ms, drop rate above 50/500 basis points, input latency above
50ms/120ms, or 3 missed heartbeats — first number is the `degraded`
threshold, second is `critical`.

A `HealthState` field of `null` in a canonical record means the health
subsystem had no assessment available yet (for example immediately after
startup), **not** that health is `ok`. Never treat a missing/`null` health
value as healthy.

## 2. Canonical JSON Lines schema (schema-v1)

Every Pier and Deck process writes one JSON object per line to its canonical
log file (and, where noted in §4, mirrors selected fields into the native OS
log). All Arcen processes on all platforms use the identical set of
top-level keys in the identical order — there is one schema, not a
per-platform fork.

### 2.1 Top-level keys

| Key | Type | Always present? | Notes |
| --- | ---- | ---------------- | ----- |
| `schema_version` | integer | yes | Currently `1`. |
| `timestamp` | string | yes | RFC 3339 UTC with microseconds, e.g. `2026-07-24T16:00:00.000000Z`. |
| `sequence` | integer | yes | Monotonic counter, **per process, starting at 1 for the first record emitted after each process start** and incrementing by 1 for every subsequent record from that same process — not a global counter across processes or across restarts. Use `(host, pid-equivalent, sequence)` together, not `sequence` alone, to detect gaps. |
| `profile_level` / `profile_name` | integer / string | yes | The minimum operational profile required for **this record** to be emitted. For a stable lifecycle event this is fixed per event ID (§3) and can be higher than the event's own severity would suggest. For an ad-hoc diagnostic (no `event_id`) it is derived directly from the emitted severity (`error`→0, `warn`→1, `info`→2, `debug`→3). This field is **not** the process's currently configured profile — use `EFFECTIVE_PROFILE` (§1.2) for that. |
| `severity` | string | yes | One of `debug`, `info`, `warn`, `error` — the true severity of this specific record, independent of `profile_level`. |
| `role` | string | yes | `host` (Pier) or `client` (Deck). (`gateway` is a reserved value for a future relay component; nothing emits it today.) |
| `component` | string | yes | Free-form bounded lowercase identifier (≤32 bytes), not a closed enum. Observed values: `pier` (Linux broker), `pier_broker` / `pier_diagnostic` (Windows broker/diagnostic processes — note the platform-specific string, same schema key), `session_agent` (per-session process, both platforms), `deck` (macOS client). |
| `platform` | string | yes | `linux`, `windows`, or `macos`. |
| `target` | string | yes | Canonical subsystem tag, always prefixed `arcen::` (e.g. `arcen::session`, `arcen::net`, `arcen::health`, `arcen::tls`, `arcen::display`, `arcen::media`, `arcen::capenc`, `arcen::hid`/`arcen::input`, `arcen::telemetry`, `arcen::auth`, plus platform-only tags such as `arcen::cppipe` and `arcen::eventlog` on Windows and `arcen::ui`/`arcen::audio` where applicable). |
| `event_id` | integer | **omitted** for ad-hoc diagnostics | Present only for stable lifecycle events (§3). Field is entirely absent from the JSON object when there is no lifecycle event — it is not serialized as `null`. |
| `event_name` | string | omitted with `event_id` | e.g. `SESSION_AUTH_OK`. |
| `category` | string | omitted with `event_id` | e.g. `machine_authentication` (§3). |
| `outcome` | string | omitted with `event_id` | One of `started`, `succeeded`, `failed`. |
| `sid` | string or `null` | always present, nullable | Correlation ID for one session. Present on every record tied to a session; `null` for process-scoped records emitted before/without a session (e.g. `SERVICE_START`). |
| `user` | string or `null` | always present, nullable | Identity string when known (e.g. `DOMAIN\artist`); `null` when not applicable to this record. |
| `host` | string or `null` | always present, nullable | Host identifier. |
| `peer_addr` | string or `null` | always present, nullable | Remote socket address when applicable. |
| `health_state` | string or `null` | always present, nullable | `ok`, `degraded`, `critical`, or `null` (no assessment yet — see §1.3). |
| `message` | string | yes | Bounded (≤512 bytes) human-readable summary. **Never** parse this for machine facts — every fact it could contain is already a structured field or a dedicated top-level key. |
| `fields` | object | yes | Bounded structured payload (see §2.3); `{}` when a record has none. |

### 2.2 Worked example

This is the frozen golden-fixture record shipped in
`shared/telemetry/tests/fixtures/canonical-record-v1.jsonl`:

```json
{"schema_version":1,"timestamp":"2026-07-24T16:00:00.000000Z","sequence":42,"profile_level":0,"profile_name":"critical","severity":"info","role":"host","component":"pier","platform":"windows","target":"arcen::session","event_id":1100,"event_name":"SESSION_AUTH_OK","category":"machine_authentication","outcome":"succeeded","sid":"canonical-session-correlation-id","user":"DOMAIN\\artist","host":"pier-01","peer_addr":"192.0.2.10:54000","health_state":"ok","message":"session authentication succeeded","fields":{"auth_method":"password","display_backend":"dxgi"}}
```

Note: the `fields.display_backend` value `"dxgi"` in this fixture is an
illustrative placeholder from the test data, not a value ever produced by
shipped code — see §2.4 for the actual observed `display_backend` and
`encoder_backend` strings.

### 2.3 The `fields` object

- Keys are bounded lowercase `snake_case` (≤64 bytes); values are booleans,
  signed integers, or bounded strings (≤512 bytes, no control characters).
- At most 16 fields per record.
- Keys are always serialized in sorted (`BTreeMap`) order, so the same event
  always produces byte-identical field ordering across platforms.
- Reserved keys `sid`, `user`, `host`, `peer_addr` cannot be reused inside
  `fields` (they already exist as top-level keys).
- Keys containing `password`, `secret`, `token`, `credential`,
  `authorization`, `cookie`, `passphrase`, `private_key`, `key_path`, or
  `session_key` (as a substring) are rejected outright — the schema itself
  refuses to accept secret-shaped field names.
- Which fields exist, and whether each is required or optional for a given
  event, is fixed per event ID (§3). An optional field that has no value is
  omitted from `fields` entirely (same omit-not-null rule as §2.1), not sent
  as `null`.

### 2.4 Same key, platform-specific value — never a schema fork

The schema key stays identical everywhere; only the string a given platform
puts into it differs. Real, observed examples:

| Field | Linux value(s) | Windows value(s) |
| ----- | --------------- | ------------------ |
| `display_backend` | `nvctrl` | `change-display-settings-ex-temporary`, `change-display-settings-ex-temporary-fallback`, `set-display-config-plus-exact-devmode`, `nvapi-purge-plus-set-display-config-exact` |
| `encoder_backend` | `native-nvenc`, `openh264-sw-h264` | `native-nvenc`, `openh264-sw-h264` |
| `component` (broker) | `pier` | `pier_broker` |

A monitoring rule that filters on `event_id`/`event_name`/`category` and
treats `fields.*` values as opaque strings works unchanged across platforms.
A rule that hard-codes an expected `fields` *value* (e.g. `display_backend
== "nvctrl"`) is inherently platform-specific — build such rules per
platform, not as a cross-platform assumption.

## 3. Stable event-ID table

Event IDs, names, categories, and outcomes are append-only and never
renumbered or reused (`shared/telemetry/src/lifecycle.rs`,
`#[non_exhaustive]`). Build alerting on `event_id` (numeric, most stable) or
`event_name` (human-readable, equally stable) plus `outcome`/`severity` —
**never** on the free-text `message`.

### 3.1 Alert classification rule

This guide classifies every event using its shipped `severity` alone, since
that is what the source itself uses to distinguish "operator must act" from
"routine":

- **`error` severity → page/alert.** Something failed and needs a human.
- **`warning` severity → investigate.** Degraded or interrupted, worth a
  ticket but not necessarily a page.
- **`information` severity → audit-only.** Expected lifecycle noise; useful
  for correlation and support-bundle context, not for alerting.

### 3.2 ⚠ Gating gotcha: some alert-worthy events are invisible at the production default

`profile_level` (§2.1) is set **per event**, independent of its severity. At
the Level-0 (Critical) production default, the following `warning`- or
`error`-severity events are **not emitted at all** because their own
`profile_level` requires Level 1 (Error) or higher. If you rely on any of
these for alerting, you must raise the monitored process to at least Level 1
(see §1.2) — Level 0 alone will not surface them:

| Event ID | Name | Severity | Requires level ≥ |
| -------- | ---- | -------- | ----------------- |
| 1202 | `DISPLAY_RESTORE_DEGRADED` | warning | 1 (Error) |
| 1401 | `TLS_CERTIFICATE_EXPIRING` | warning | 1 (Error) |
| 1506 | `CLIENT_RECONNECT` | warning | 1 (Error) |
| 1604 | `HID_PASSTHROUGH_ERROR` | **error** | 1 (Error) |
| 1702 | `NETWORK_PATH_LOST` | warning | 1 (Error) |
| 1901 | `PERMISSION_DENIED` | warning | 1 (Error) |
| 1902 | `PERMISSION_REVOKED` | warning | 1 (Error) |

`HID_PASSTHROUGH_ERROR` (1604) is the notable case: it is `error`-severity
(a page/alert-class event by the rule in §3.1) yet is still gated behind
Level 1 at the default Level 0 profile. If your alerting is built only
against Level-0 log output, you will never see it fire.

### 3.3 Full event table

| ID | Name | Category | Outcome | Severity | Min. level | Class |
| -- | ---- | -------- | ------- | -------- | ---------- | ----- |
| 1000 | `SERVICE_START` | health | succeeded | information | 0 | audit-only |
| 1001 | `SERVICE_STOP` | health | succeeded | information | 0 | audit-only |
| 1002 | `SERVICE_FAILED` | health | failed | **error** | 0 | **page/alert** |
| 1100 | `SESSION_AUTH_OK` | machine_authentication | succeeded | information | 0 | audit-only |
| 1101 | `SESSION_AUTH_FAIL` | machine_authentication | failed | warning | 0 | investigate |
| 1102 | `SESSION_STREAM_START` | streaming | succeeded | information | 0 | audit-only |
| 1103 | `SESSION_END` | cleanup | succeeded | information | 0 | audit-only |
| 1104 | `SESSION_INTERRUPTED` | reconnect | failed | warning | 0 | investigate |
| 1200 | `DISPLAY_ARMED` | health | started | information | 2 | audit-only |
| 1201 | `DISPLAY_RESTORED` | cleanup | succeeded | information | 2 | audit-only |
| 1202 | `DISPLAY_RESTORE_DEGRADED` | cleanup | succeeded | warning | 1 | investigate (gated, §3.2) |
| 1203 | `DISPLAY_RESTORE_FAILED` | cleanup | failed | **error** | 0 | **page/alert** |
| 1204 | `WATCHDOG_RESTORE` | cleanup | succeeded | warning | 0 | investigate |
| 1300 | `CP_LOGON_OK` | machine_authentication | succeeded | information | 0 | audit-only |
| 1301 | `CP_LOGON_FAIL` | machine_authentication | failed | warning | 0 | investigate |
| 1400 | `TLS_CERTIFICATE_ACTIVE` | health | succeeded | information | 0 | audit-only |
| 1401 | `TLS_CERTIFICATE_EXPIRING` | health | started | warning | 1 | investigate (gated, §3.2) |
| 1402 | `TLS_CERTIFICATE_RELOADED` | health | succeeded | information | 2 | audit-only |
| 1403 | `TLS_CERTIFICATE_RELOAD_FAILED` | health | failed | **error** | 0 | **page/alert** |
| 1404 | `TLS_CERTIFICATE_EXPIRED` | health | failed | **error** | 0 | **page/alert** |
| 1500 | `CLIENT_START` | health | succeeded | information | 0 | audit-only |
| 1501 | `CLIENT_STOP` | health | succeeded | information | 0 | audit-only |
| 1502 | `CLIENT_CONNECT_ATTEMPT` | connection | started | information | 2 | audit-only |
| 1503 | `CLIENT_CONNECT_OK` | connection | succeeded | information | 0 | audit-only |
| 1504 | `CLIENT_CONNECT_FAIL` | connection | failed | **error** | 0 | **page/alert** |
| 1505 | `CLIENT_SESSION_END` | cleanup | succeeded | information | 0 | audit-only |
| 1506 | `CLIENT_RECONNECT` | reconnect | started | warning | 1 | investigate (gated, §3.2) |
| 1600 | `HID_DEVICE_ATTACHED` | peripheral | succeeded | information | 2 | audit-only |
| 1601 | `HID_DEVICE_DETACHED` | peripheral | succeeded | information | 2 | audit-only |
| 1602 | `HID_PASSTHROUGH_START` | peripheral | started | information | 2 | audit-only |
| 1603 | `HID_PASSTHROUGH_END` | peripheral | succeeded | information | 2 | audit-only |
| 1604 | `HID_PASSTHROUGH_ERROR` | peripheral | failed | **error** | 1 | **page/alert** (gated, §3.2) |
| 1700 | `NETWORK_PATH_ACTIVE` | network | succeeded | information | 2 | audit-only |
| 1701 | `NETWORK_PATH_CHANGED` | network | succeeded | information | 2 | audit-only |
| 1702 | `NETWORK_PATH_LOST` | network | failed | warning | 1 | investigate (gated, §3.2) |
| 1703 | `NETWORK_PATH_RESTORED` | network | succeeded | information | 1 | audit-only (gated) |
| 1800 | `HEALTH_OK` | health | succeeded | information | 0 | audit-only |
| 1801 | `HEALTH_DEGRADED` | health | started | warning | 0 | investigate |
| 1802 | `HEALTH_CRITICAL` | health | failed | **error** | 0 | **page/alert** |
| 1803 | `HEARTBEAT_LOST` | health | failed | **error** | 0 | **page/alert** |
| 1804 | `TELEMETRY_DROPPED` | telemetry | failed | warning | 0 | investigate |
| 1805 | `EFFECTIVE_PROFILE` | telemetry | succeeded | information | 0 | audit-only |
| 1806 | `HEALTH_SNAPSHOT` | health | succeeded | information | 0 | audit-only (proof-of-life, §1.3) |
| 1900 | `PERMISSION_GRANTED` | permission | succeeded | information | 2 | audit-only |
| 1901 | `PERMISSION_DENIED` | permission | failed | warning | 1 | investigate (gated, §3.2) |
| 1902 | `PERMISSION_REVOKED` | permission | failed | warning | 1 | investigate (gated, §3.2) |
| 1903 | `PERMISSION_PENDING` | permission | started | information | 2 | audit-only |

## 4. Where the records live

### 4.1 Linux (Pier)

- Canonical JSONL: `/var/log/arcen/arcen-pier.log`, mode `0640`, owned
  `root:root`.
- Rotation: packaged `logrotate` policy
  (`packaging/linux/arcen-pier.logrotate`) rotates **daily or at 32 MiB**,
  whichever comes first, keeps 100 generations, compresses with a one-cycle
  delay, and names archives
  `arcen-pier.log-YYYYMMDD-<unix-epoch>[.gz]`. After rotation it sends
  `systemctl kill --kill-who=main -s HUP arcen-pier.service` so the process
  reopens its file handle. The process also runs its own internal
  size/age-based maintenance sweep (archive at ≥32 MiB or ≥24h; delete
  archives older than the configured retention, default 30 days, bounded
  7-100) as a backstop independent of `logrotate`.
- Reload: `sudo systemctl reload arcen-pier` (see §1.2).
- Native mirror — journald: every record that has an `event_id` (i.e. every
  stable lifecycle event, never ad-hoc diagnostics) is also sent to journald
  with `SYSLOG_IDENTIFIER=arcen-pier`, `PRIORITY` mapped from severity
  (error→3, warn→4, info→6, debug→7), and structured fields
  `ARCEN_EVENT_ID`, `ARCEN_EVENT_NAME`, `ARCEN_CATEGORY`, `ARCEN_OUTCOME`,
  `ARCEN_SEVERITY`, `ARCEN_CORRELATION_ID` (when a `sid` is present), and one
  `ARCEN_FIELD_<UPPERCASE_KEY>` per structured field, in sorted order. When
  journald is unavailable, output falls back to `/dev/log` using classic RFC
  3164 syslog framing (no RFC 5424 structured data in v1).
- Sensitive paths: `/run/arcen/sessions/**` (session runtime state, e.g.
  `Xorg.log`/`Xauthority`) is never traversed by log or support-bundle
  collection and should not be exposed to monitoring agents.

### 4.2 Windows (Pier)

- Canonical JSONL (broker): `%ProgramData%\Arcen\logs\arcen-pier.log`.
- Canonical JSONL (per-session agent):
  `%ProgramData%\Arcen\logs\sessions\arcen-session-agent-<sid>.log`.
- Rotation: the broker runs its own maintenance pass at startup and every 24
  hours (internal size/age thresholds identical to Linux, §4.1). Archives
  land under `logs\archive` (broker) and `logs\archive\sessions`
  (per-session), named `arcen-pier-<unix-epoch>[-<n>].log` and
  `arcen-session-agent-<sid>-<unix-epoch>[-<n>].log` respectively (the
  `-<n>` numeric suffix only appears on a same-second collision).
- Reload: `sc.exe control ArcenPier 201` (reload configured profile/QoS) or
  `sc.exe control ArcenPier 200` (temporary Level 3 debug) — see §1.2.
- Native mirror — Windows Event Log: provider `ArcenPier`, channel
  `Application`. **No message-table DLL is registered** for this provider
  (confirmed: `packaging/windows/host/eventlog-source.ps1` never writes an
  `EventMessageFile`/`CategoryMessageFile` registry value), so Event Viewer
  cannot render a formatted description — it will show a "description not
  found" note. All the facts are still there as raw, deterministic
  insertion strings: element 0 is a human summary
  (`ArcenPier lifecycle event {id} {NAME} ({severity})`), and the remaining
  elements are sorted `key=value` pairs including `event_id`, `event_name`,
  `category`, `outcome`, `severity`, `correlation_id`, and every schema
  field. The numeric `EventID` in Event Viewer is the lifecycle `event_id`
  itself, so filtering/alerting by `EventID` works without any message
  table.
- Permissions: the broker log is written by the service account; session
  logs are ACL'd per Windows session and are not readable across sessions
  without administrative rights.

### 4.3 macOS (Deck)

- Canonical JSONL: `~/Library/Logs/Arcen/arcen-client.log`, rolled daily.
- Overridable log directory (all platforms, dev/support use): `ARCEN_LOG_DIR`
  environment variable.
- Reload: the "Log level" control in the Deck UI applies immediately — there
  is **no config-file watcher** on macOS; changing a config file on disk
  while the app is running has no effect until the app restarts or the UI
  control is used.
- Native mirror: file only. There is no journald/Event-Log equivalent
  mirrored on macOS in the current build.

## 5. Reusable monitoring recipes

### 5.1 Generic JSONL tailing

```bash
tail -F /var/log/arcen/arcen-pier.log | while read -r line; do
  echo "$line" | jq -c 'select(.severity == "error")'
done
```

### 5.2 jq

```bash
# All page/alert-class events (error severity) since the last rotation
jq -c 'select(.severity == "error")' /var/log/arcen/arcen-pier.log

# One session's full timeline, by correlation id
jq -c --arg sid "canonical-session-correlation-id" 'select(.sid == $sid)' \
  /var/log/arcen/arcen-pier.log

# Count events by event_name
jq -r 'select(.event_name != null) | .event_name' /var/log/arcen/arcen-pier.log \
  | sort | uniq -c | sort -rn
```

### 5.3 JSONPath

Any JSONPath-capable log platform can select on the same flat keys, e.g.
`$[?(@.event_id==1802)]` for `HEALTH_CRITICAL`, or
`$[?(@.severity=="error" && @.role=="host")]` for host-side page/alert
events.

### 5.4 Regex (when a JSON-aware tool is unavailable)

Because the schema field order and keys are frozen, a conservative
line-anchored regex is stable enough for basic triage tools that cannot
parse JSON:

```
"event_id":1802         # matches only HEALTH_CRITICAL records
"severity":"error"      # matches every page/alert-class record
```

Never regex-match against `"message":"..."` content for anything you can get
from `event_id`/`event_name`/`fields` instead — message text is not part of
the stable contract and may be reworded at any time.

### 5.5 Zabbix `log[]` / `logrt[]` (concept only, no version-specific claims)

Arcen log files are plain newline-delimited JSON text files, so they work
with Zabbix's standard active-agent log-monitoring items. Two applicable
item keys:

- `log[<file>,<regexp>,<encoding>,<maxlines>,<mode>,<output>,<maxdelay>]` —
  monitors one fixed file (matches Windows' single broker log path).
- `logrt[<file_regexp>,<regexp>,<encoding>,<maxlines>,<mode>,<output>,<maxdelay>]`
  — monitors the newest file matching a filename pattern, which is the
  correct choice on Linux where `logrotate` renames the active file (use a
  pattern matching `arcen-pier\.log$`, not the dated archives, so the item
  keeps following the currently-active file across rotation).

Both are **active-agent-only** items (there is no passive/"agent test"
equivalent), and neither key decompresses `.gz` archives — they only see the
live, uncompressed file, which matches Linux's `delaycompress` rotation
policy.

A conceptual template item, with the path expressed as an overridable user
macro so the same template works across a Linux install path and a custom
override, without hard-coding a host-specific path into every item (this is
descriptive documentation of the pattern, not shipped Zabbix
import/export YAML):

```
Macro:  {$ARCEN_PIER_LOG_PATH}   Default value: /var/log/arcen/arcen-pier.log

Item key:
  logrt[{$ARCEN_PIER_LOG_PATH},"\"severity\":\"error\"",,,skip,,]

Trigger expression (conceptual):
  count(/Arcen Pier/logrt[{$ARCEN_PIER_LOG_PATH},"\"severity\":\"error\"",,,skip,,],#1)>0
```

Use `mode = skip` so Zabbix only evaluates lines appended since the last
check (matching the append-only nature of the canonical log), and capture
`event_id`/`event_name` via the item's output/regex capture group instead of
alerting on raw text where possible.

### 5.6 journald queries (Linux)

```bash
# Follow only lifecycle events, formatted for a human
journalctl -t arcen-pier -f -o cat

# Everything at page/alert severity, structured JSON
journalctl -t arcen-pier -o json --no-pager | jq -c 'select(.ARCEN_SEVERITY == "error")'

# Filter by stable event ID, most recent first (the exact invocation Arcen's
# own support-bundle collector uses internally)
journalctl -t arcen-pier -o json --no-pager -r -n 200 \
  | jq -c 'select(.ARCEN_EVENT_ID == "1802")'
```

### 5.7 Windows Event Viewer / PowerShell

```powershell
# Last 20 ArcenPier events of any kind
Get-WinEvent -LogName Application `
  -FilterXPath "*[System[Provider[@Name='ArcenPier']]]" |
  Select-Object -First 20 TimeCreated, Id, LevelDisplayName, Message

# Only HEALTH_CRITICAL (event ID 1802)
Get-WinEvent -LogName Application `
  -FilterXPath "*[System[Provider[@Name='ArcenPier'] and (EventID=1802)]]"

# Every page/alert-class event: Windows Error level maps 1:1 to Arcen's
# error severity for lifecycle events
Get-WinEvent -LogName Application `
  -FilterXPath "*[System[Provider[@Name='ArcenPier'] and (Level=2)]]"
```

Because no message-table DLL is registered (§4.2), filter on `EventID`
and/or `Level` rather than any rendered description text, and read the raw
`key=value` insertion strings in `Message` for `event_name`/`fields`.

### 5.8 Loki / Elastic ingestion

Both ingest newline-delimited JSON directly with no transform:

- **Loki (Promtail)**: a `json` pipeline stage mapping `event_id`,
  `event_name`, `severity`, `sid`, and `role`/`component`/`platform` to
  labels (keep label cardinality bounded — do not label on `sid` or
  `sequence`), with the full line kept as the log line for later
  jq/LogQL-style filtering.
- **Elastic (Filebeat)**: a JSON input (`json.keys_under_root: true`) reading
  the canonical file directly, since every top-level key is already a valid
  flat Elasticsearch field name; no ingest-pipeline grok pattern is needed
  because there is no free-text to parse.

## 6. Host-side end-to-end diagnosis

- **The host log is the end-to-end story for one session.** Every Pier-side
  record for a session — auth, stream start, QoS rollups, health state,
  network path changes, session end — carries the same `sid`. The Deck-side
  QoS/network samples for that same session ride to the host over the
  existing session transport (`HealthPingMsg.client_telemetry`, every 5
  seconds) and are folded into the host's own health rollup, so **the host's
  own log already reflects the client's reported experience** without you
  needing to correlate two separate log streams by timestamp guessing.
- **Health disagreement is a real diagnostic signal.** If the host reports
  `health_state: "ok"` while a user reports a bad experience, or vice versa,
  that disagreement itself points at the network path between Pier and Deck
  (delivery succeeded per the host's own measurement but did not translate
  to a good client experience, or the reverse) — check `NETWORK_PATH_*`
  events (1700-1703) around the same `sid` and timeframe before assuming a
  measurement bug.
- **Legacy per-encoder-frame telemetry is not part of the canonical
  schema and is not persisted.** The `capenc` helper process (the
  platform-specific screen-capture/encode child) writes its own pre-schema,
  free-text stats line to its own stdout pipe (`enc_fps=… avg_encode_ms=…
  kbps=…`), consumed internally by the host process to compute the
  `fps_actual`/`fps_target`/etc. fields surfaced in `HEALTH_SNAPSHOT` (1806).
  The raw per-frame line itself is not written to the canonical log,
  journald, Event Log, or the support bundle — if you need that level of
  encoder detail you must reproduce the issue live and rely on the derived
  `HEALTH_SNAPSHOT`/QoS fields, not go looking for the raw line after the
  fact.
- **No remote log-level control exists** (§1.2). A host cannot be made more
  verbose by anything the client does, and vice versa; each side's profile
  is a purely local decision.

## 7. Incident workflows

### 7.1 Who, when, from where

Answer with `SESSION_AUTH_OK`/`SESSION_AUTH_FAIL` (1100/1101,
`fields.auth_method`, `user`, `peer_addr`) for identity and origin, and
`CLIENT_CONNECT_OK`/`CLIENT_CONNECT_FAIL` (1503/1504) plus
`CLIENT_START` (1500, `fields.version`/`os`/`arch`) on the Deck side for
build/platform context. All are correlated by the shared `sid`.

### 7.2 Working / degraded / critical

Use `health_state` and the `HEALTH_OK`/`HEALTH_DEGRADED`/`HEALTH_CRITICAL`
events (1800-1802) as the three-state summary; treat `health_state: null` as
unknown, not healthy (§1.3). `HEALTH_SNAPSHOT` (1806) gives you the
underlying `fps_actual`/`rtt_ms`/`drop_basis_points`/`heartbeat_misses`
numbers behind the state, every 60 seconds even at the production default.

### 7.3 Network path changes

The four `NETWORK_PATH_*` events (1700-1703) each carry a different, fixed
set of fields — do not assume one shape applies to all four:

- `NETWORK_PATH_ACTIVE` (1700): required `interface_kind`, `scope`; optional
  `link_mbps`, `ssid`, `rssi_dbm`, `mtu`.
- `NETWORK_PATH_CHANGED` (1701): required `old_kind`, `new_kind`; optional
  `old_mbps`, `new_mbps`, `reason_class`.
- `NETWORK_PATH_LOST` (1702): required `interface_kind` only — no `scope`,
  `ssid`, or link-speed fields.
- `NETWORK_PATH_RESTORED` (1703): required `interface_kind` and `gap_ms`
  (how long the path was down).

Remember `NETWORK_PATH_LOST` (1702) requires Level 1 to be visible at all
(§3.2) — if your incident timeline is missing a path-loss event you expect,
confirm the process's effective profile first via `EFFECTIVE_PROFILE`
(1805) before concluding the network was actually stable.

### 7.4 Telemetry loss

`TELEMETRY_DROPPED` (1804, `fields.sink`, `fields.dropped_count`) indicates
the process itself could not keep up with its own bounded telemetry sink —
treat any gap in `sequence` numbers, or a `TELEMETRY_DROPPED` record, as
"the record you're missing is unrecoverable," not as evidence nothing
happened. Cross-check `HEALTH_SNAPSHOT` cadence (§1.3): if 60-second
snapshots stop arriving entirely, treat the process as unresponsive.

### 7.5 Export and support-bundle privacy

For a full timeline handoff (support escalation, post-incident review),
collect a support bundle rather than hand-copying raw log files — canonical
JSONL records are pseudonymized per bundle (identity/user/host values are
replaced with a keyed `anon:`-prefixed token, not removed, so the same
identity still correlates across records within one bundle without
revealing the real value), and native OS log excerpts included in the
bundle are separately redacted of secrets/TLS material. See
[`support-bundles.md`](support-bundles.md) for exact collection commands,
storage location, and the full redaction contract. Do not paste raw
`user`/`peer_addr`/session-log content (which may include native-OS
identity strings) into tickets or chat; use the pseudonymized bundle export
instead.
