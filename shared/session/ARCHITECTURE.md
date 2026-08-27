# `arcen-session` Architecture

**Status (2026-08-25):** active. Default features expose the pure,
dependency-light restore-lease, direct-reconnect, and Deskside surfaces.

## Default surface

`restore_lease` has no host, client, transport, clock, filesystem, async, or FFI
behavior. It provides:

- `IanaTimeZone`, with deterministic ASCII path-segment validation and a
  128-byte bound. This proves syntax only; host adapters must validate semantic
  existence and containment.
- `LeaseOwnerId`, bounded to 128 bytes.
- `StateFingerprint`, a SHA-256 digest over at most 64 KiB of adapter-provided
  snapshot bytes.
- `RestoreLease`, `RestoreResource`, `RestorePhase`, `RestoreEvent`, and
  `RecoveryDirective`, which model durable decisions while adapters own
  persistence and mutation.

`direct_reconnect` emits exact generation-tagged deadline, media/input reset,
restore-hold, and terminal-restore actions without owning a clock or I/O.

`deskside` adds `DesksidePolicy`, a validated hash-only
`PhysicalHostEvidence`, `DesksideDecision`, and `DesksideProtection`. Required
mode always owns input and display together. The composite emits one bounded
arm/apply/verify/restore effect at a time, reverses partial arm in display-then-
input order, holds unchanged through approved reconnect, stops remote injection
before terminal restore, attempts both restores after a failure, and authorizes
cleanup only after success. Failed restore remains observable for the adapter's
journal/watchdog.

The default dependency set is limited to serialization and hashing support.
Shared code never depends on a product crate.

## Invariants and recovery semantics

1. The host durably arms a lease before beginning an OS mutation.
2. Phases are explicit: `armed`, `applying`, `applied`, `restoring`,
   `restored`, or `conflicted`.
3. Repeating the current begin/success event is idempotent. This permits
   at-least-once journal writes and recovery retries without pretending OS
   operations are exactly once.
4. In an active mutation phase, current state equal to the target directs a
   restore; current state equal to the original directs journal removal.
5. Current state matching neither fingerprint is never overwritten. Recovery
   marks the lease conflicted and holds the journal for operator adjudication.
6. An armed lease means mutation did not begin; a restored lease means restore
   was confirmed. In both cases the adapter may remove the journal.
7. Invalid transitions return an error and leave the phase unchanged.

The model does not contain the original state bytes or perform disarm I/O.
Windows and Linux persist exact host-owned snapshots beside the lease. Shared
state contains only bounded normalized fingerprints and ordering.
