# `arcen-session` Architecture

**Status (2026-11-09):** active. Default features expose the pure,
dependency-light session policy every Pier and Deck shares: restore leases,
direct reconnect and Deskside below, and the modules in the map that follows.

## Module map

| Module | Owns |
| --- | --- |
| `agent_relay` | The contract between a Pier's network service and its per-session desktop agents. |
| `auth_throttle` | Backing off repeated failed sign-ins, per source address and per account (used by the macOS Pier). |
| `connection_diagnostics` | The steps, typed `diag.*` findings and wording behind `arcen-deck diagnose` and the Deck's **Test connection**; platforms only probe. |
| `session_admission` | Capacity-one Pier admission and the bounded reconnect hold. |
| `setup_budget` | One setup budget for a Pier's broker and its per-session agent (30 s broker wait, 25 s for the agent's own waits), so a slow setup ends in a typed refusal instead of a terminated agent. |
| `login_window_handover` | Following a sign-in screen into the desktop that replaces it. |
| `local_playback` | Silencing the host's own speakers for a session, and the evidence that proves it. |
| `pier_config` | The shared, platform-parameterised `pier.json` schema. |
| `session_locale`, `zoneinfo` | The locale of a host-started desktop and IANA time-zone validation for redirection. |
| `host_lifecycle`, `install_lifecycle` | OS-free Pier and installer lifecycle state machines. |
| `network_reachability` | Classification of outbound reachability failures. |
| `desktop_provider`, `install_provider` | Linux desktop choice (X11 or Wayland) from host facts, at start and at install. |
| `x11_seat` | One X11 desktop per host, on `seat0`. |
| `polkit_remote` | Keeping remote desktops remote to polkit. |
| `wayland_qualification`, `wayland_session`, `wayland_outputs`, `wayland_x11`, `desktop_session`, `native_seat`, `compositor_prefix` | The headless and native Wayland provider's policy: the qualification record, session readiness, virtual outputs, Xwayland, the GNOME session parts Arcen assembles, the native seat and an optional side-by-side GNOME prefix. |

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
