# shared/telemetry — `arcen-telemetry`

**Delivery:** pure, platform-free telemetry contracts consumed by Arcen products.

## Session correlation

`CorrelationId` is the validated value object for diagnostic correlation across
process and transport boundaries. Session-scoped tracing uses the stable field
key `sid`; reconnect bridges use `previous_sid`.

The Deck and Piers own random-byte acquisition. The pure crate only validates
canonical UUID text and formats caller-supplied bytes as UUID v4, so it has no
I/O, clock, environment, filesystem, or random-number dependency.

Correlation IDs are diagnostic metadata, not secrets or authority. They never
authenticate a user, authorize a session, select a machine, or replace native
WTS/logind identity.

## Log policy

`VerbosityTier` provides stable numeric tiers `0..=3`, and `LevelSpec` maps each
tier to the common `warn,arcen=<level>` tracing directive. Host adapters may
append their own module target or replace the directive with a valid
`ARCEN_LOG` override.

`plan_log_maintenance` accepts at most 4096 clockless, pathless metadata
records. It deterministically plans active-file archives and expired
archive/session deletions by host-owned `LogFileId`; hosts retain all
filesystem, clock, locking, signal, and standard-handle responsibilities.
Retention defaults to 30 days and is normalized to 7–100 days.

## Lifecycle events

`LifecycleEventKind` defines the append-only IDs `1000..=1301` for Pier service,
authentication, streaming, display restore, watchdog, and Credential Provider
outcomes. `LIFECYCLE_EVENT_DEFINITIONS` is the canonical ID/name/category/
outcome/severity and closed-field schema table; fleet rules key on stable IDs
and names rather than localized text.

`ValidatedLifecycleEvent::new` derives canonical category and outcome and
rejects missing, undeclared, or mistyped fields. `StructuredFields` accepts at
most 16 entries, rejects control characters and secret-adjacent key names, and
keeps string values bounded. The source-compatible raw `LifecycleEvent` remains
available, but native host sinks accept only the validated wrapper.

The pure crate performs no I/O. Windows and Linux own best-effort native
adapters, and their failures never change authentication, session, media,
display, or cleanup outcomes.

## Support bundle contract

`support_bundle` defines the platform-free schema-v1 manifest, validated relative
archive paths, SHA-256 representation, typed collection notices, truncation
metadata, and recursive JSON redaction used by both Pier hosts. The deterministic
builder rejects `manifest.json` as a payload entry, sorts all manifest collections,
and bounds entries (2,048), notices (4,096), and redaction records (4,096).

The shared crate does not inspect files, run commands, query native event stores,
or create archives. Host adapters own strict source allowlists and fixed-buffer
streaming. Secret-adjacent JSON keys are replaced with `[REDACTED]`; adapters
record unavailable or denied sources as typed logical notices rather than
serializing host paths or raw errors.

## Invariants

- `#![forbid(unsafe_code)]`.
- Shared-only dependency direction; no host, client, gateway, or packaging
  dependency.
- Validation occurs once at a trust boundary; emit paths borrow the validated
  string without allocating.
- Lifecycle IDs and names are append-only; definitions may be added but existing
  meanings are never changed or reused.
- Untrusted rejected values are never emitted into logs.
- Log planning is deterministic, bounded, pathless, clockless, and I/O-free.
- Support-bundle planning is deterministic, bounded, path-safe, and I/O-free.

## Interfaces

- `CorrelationId::new` preserves the general bounded correlation contract.
- `CorrelationId::parse_uuid` validates the session-log UUID wire contract.
- `CorrelationId::from_uuid_v4_bytes` formats edge-supplied random bytes.
- `Display`, `AsRef<str>`, and `as_str` expose the validated value without
  changing its semantics.
- `VerbosityTier` and `LevelSpec` define shared coarse logging policy.
- `RetentionPolicy` and `plan_log_maintenance` define bounded maintenance
  decisions over host-supplied metadata.
- `LifecycleEventKind`, `LIFECYCLE_EVENT_DEFINITIONS`, and
  `ValidatedLifecycleEvent` define the stable closed lifecycle vocabulary.
- `SupportBundleBuilder`, `SupportBundleManifest`, `BundlePath`, and
  `redact_json_document_at` define the shared bundle manifest and privacy
  contract.
