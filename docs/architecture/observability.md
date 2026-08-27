# Observability Architecture

> **Status (2026-07-25): implemented in
> its implementation review.**
> Design was approved in the design review.
> This is not a release or production-acceptance claim. Target-native Windows
> execution and Linux operational acceptance remain platform gates.

This record describes the implemented direct-connection observability surface
for the macOS Deck and Linux/Windows Piers. The retained
[build plan](ObservabilityStandard.md) contains design rationale and
acceptance history; this document records the resulting boundaries and points to
their implementations.

## Boundaries

Observability has two shared layers:

- [`arcen-telemetry`](../../shared/telemetry/src/lib.rs) is the pure,
  I/O-free contract. It owns operational profiles, the append-only lifecycle
  vocabulary, canonical names and records, QoS/health decisions, network value
  objects, retention policy, and support-bundle transformation.
- [`arcen-observability`](../../shared/observability/src/lib.rs) is the OS-free
  runtime. It owns tracing integration, canonical routing, profile reload,
  off-hot-path sampling, fixed-capacity workers, loss accounting, and bounded
  flush/shutdown.

OS APIs, protected file locations, native log protocols, service/UI controls,
and network probes remain in the products. Wire shapes remain in
[`arcen-protocol`](../../shared/protocol/src/messages.rs). This preserves
one-way dependency direction: products consume shared contracts; shared crates
do not consume products.

## Profiles, severity, and canonical records

[`OperationalProfile`](../../shared/telemetry/src/log_policy.rs) is a cumulative
quantity dial, independent of an event's diagnostic severity:

| Level | Profile | Adds |
| --- | --- | --- |
| 0 | Critical | Mandatory service/session state, health transitions, loss notices, and proof of life |
| 1 | Error | Investigable failures, warnings, retries, denials, and degraded components |
| 2 | Info | Routine lifecycle, network/device changes, and aggregate QoS |
| 3 | Debug | Bounded diagnostic detail, never per-frame, per-packet, HID-report, or input-event logging |

The built-in and packaged production default is Level 0. Named development/lab
configuration uses Level 2. `ARCEN_LOG` may refine ordinary diagnostic tracing
but cannot suppress mandatory Level-0 canonical records. Local reloads update
the process profile; profile control never crosses the network.

[`CanonicalRecord`](../../shared/telemetry/src/schema.rs) freezes JSONL
schema-v1, key order, bounds, nullable identity/health context, sorted structured
fields, per-process monotonic sequence, and the separation between
`profile_level` and `severity`. Stable lifecycle IDs and their closed field
schemas are append-only in
[`lifecycle.rs`](../../shared/telemetry/src/lifecycle.rs); canonical targets and
field names are in [`names.rs`](../../shared/telemetry/src/names.rs). Ad-hoc
diagnostics omit event identity and are not monitoring triggers. The exact
operator schema and event table are in the
[monitoring guide](../operations/monitoring.md#2-canonical-json-lines-schema-schema-v1).

## Runtime, backpressure, and shutdown

[`ObservabilityBuilder`](../../shared/observability/src/runtime.rs) requires a
canonical sink and creates a local dispatch before any optional process-global
installation. [`BoundedSink`](../../shared/observability/src/sink.rs) gives each
sink a dedicated worker and fixed-capacity queue; producers use `try_send` and
never wait for disk, journald, Event Log, or console I/O. Default capacities are
1,024 canonical records and 256 console records.

Queue-full, queue-closed, delivery, and flush failures have separate monotonic
loss counters. Heartbeat code drains deltas and emits `TELEMETRY_DROPPED` (1804)
to every sink except the failing origin, avoiding recursive loss reporting.
Adapter errors mark a sink unhealthy while its worker continues. A sink panic is
caught long enough to mark the worker unhealthy, then resumed; later enqueue,
flush, and shutdown calls report closure or worker panic instead of silently
claiming delivery.

Flush and drain-and-shutdown are deadline-bounded per sink. Local runtime drop
performs a 100 ms best-effort shutdown; explicitly process-global runtimes retain
workers for process lifetime and expose bounded flush without pretending they
can be torn down through a dropped handle. Tests cover queue loss, worker panic,
global lifetime, and bounded flush under
[`shared/observability/tests`](../../shared/observability/tests/).

## Protocol and end-to-end health

Protocol v3 gained only optional, bounded facts:
`ClientHelloMsg.network_snapshot`,
`HealthPingMsg.client_telemetry`, and additive host health metadata. Missing
values mean unavailable, never zero or healthy. The existing ping sequence is
reused for application RTT. Compatibility and the prohibition on remote profile
control are normative in
[`WIRE.md`](../../shared/protocol/WIRE.md#client-qosnetwork-telemetry-protocol-v3-additive).

Hot paths update atomics; sampling and formatting occur off-path. The cadence
contract is a bounded client QoS/network snapshot on the macOS Deck's existing
5-second health ping, 2-second Pier detail feeding 10-second Level-2 aggregate
QoS reporting, and mandatory `HEALTH_SNAPSHOT` (1806) proof of life every 60
seconds at every profile. Piers evaluate host delivery together with fresh
client experience. [`health.rs`](../../shared/telemetry/src/health.rs) owns validated
thresholds and two-window hysteresis; product bookkeeping lives in the
[macOS](../../clients/macos/src/observability.rs),
[Linux](../../hosts/linux/src/observability.rs), and
[Windows](../../hosts/windows/src/observability.rs) adapters.

Client telemetry can inform a Pier's local assessment, but it cannot alter the
Pier's profile or sinks. Stale client samples become unavailable. Host and
client health disagreement remains diagnostic truth rather than being collapsed
into a guessed healthy state.

## Platform adapters and native values

| Product | Implemented adapter |
| --- | --- |
| macOS Deck | Canonical file output and UI profile reload; `getifaddrs`/Darwin `if_data` derive route interface, MTU, and link rate in [`netinfo.rs`](../../clients/macos/src/netinfo.rs). |
| Linux Pier | Canonical managed file plus bounded journald/RFC 3164 adapter; bounded `/sys/class/net` and `/proc/net/route` reads select the routed interface in [`netinfo.rs`](../../hosts/linux/src/netinfo.rs); SIGHUP reopens output and reloads profile/QoS targets. |
| Windows Pier | Broker/per-session canonical files plus bounded Application Event Log adapter; `GetAdaptersAddresses` supplies interface kind, transmit rate, and MTU in [`netinfo.rs`](../../hosts/windows/src/netinfo.rs); SCM controls retain their local reload roles. |

The schema normalizes keys, not inherent backend values. Linux
`display_backend` is `nvctrl`; Windows reports its actual
`change-display-settings-ex-temporary`,
`change-display-settings-ex-temporary-fallback`,
`set-display-config-plus-exact-devmode`, or
`nvapi-purge-plus-set-display-config-exact` path. Encoder values likewise remain
the real adapter result: `native-nvenc` and `openh264-sw-h264` on both hosts.
Monitoring rules that need
cross-platform behavior therefore use stable event IDs and keys, while rules on
backend values remain platform-specific. Paths, native-field mappings, and
current component values are maintained in
[Where the records live](../operations/monitoring.md#4-where-the-records-live).

## Identity, network privacy, and support bundles

Access-controlled local logs may carry schema-approved `user`, `host`, and
`peer_addr` values so operators can reconstruct a session. Secret-shaped field
names are rejected by the canonical schema. Network facts use the bounded pure
[`NetworkSnapshot`](../../shared/telemetry/src/network.rs), but raw SSID is
omitted by the macOS, Linux, and Windows adapters unless a future explicit local
product policy authorizes disclosure; OS read permission alone is not consent
to transmit it.

Support bundles do not copy local identities verbatim. The pure
[support-bundle contract](../../shared/telemetry/src/support_bundle.rs) uses a
fresh host-generated 256-bit key per bundle and domain-separated HMAC-SHA-256
for user, host, peer address, and network identity. It preserves correlation
within one bundle, prevents correlation across bundles, zeroes key storage on
drop, rejects malformed canonical lines rather than copying them raw, and emits
bounded `anon:` pseudonyms. Collection, residual sensitivity, and exclusions
are authoritative in
[`support-bundles.md`](../operations/support-bundles.md).

## Configuration, operations, and conformance

The shared [`LoggingConfig`](../../shared/session/src/pier_config.rs) owns the
configuration migration. `logging.level` is canonical; legacy
`logging.verbosity` is accepted for one migration release with explicit old
numeric mapping, and specifying both is an error. QoS targets are validated in
the same configuration boundary. Operator commands, paths, schema fields,
alerting guidance, and incident workflows belong in
[`monitoring.md`](../operations/monitoring.md), not in this architecture record.

[`validate_observability.py`](../../scripts/validate_observability.py) validates
bounded JSONL, canonical metadata, per-process sequences, and optional
cross-file lifecycle order/tolerances. Its
[unit tests](../../scripts/test_validate_observability.py) cover malformed and
drifted input, and rights-safe macOS→Linux and macOS→Windows fixtures live under
[`tests/e2e/observability`](../../tests/e2e/observability/) with usage in the
[E2E README](../../tests/e2e/README.md#observability-conformance).

Portable shared/runtime tests, strict linting, product adapter tests, and
synthetic conformance were completed for PR #59. The synthetic
fixtures are not hardware evidence. Target-native Windows execution and Linux
operational acceptance remain required platform gates; packaging,
signing, deployment, and release evidence remain separate.

## Explicitly deferred

- Arcen Span/gateway aggregation and relay messages
- Dormant clients and other future product surfaces
- OpenTelemetry and any off-box aggregation/export pipeline
- Raw SSID disclosure by default
- A macOS native `os_log` mirror

These items require their own architecture, privacy, operations, and platform
acceptance work. They are not implied by the completed worktree implementation.
