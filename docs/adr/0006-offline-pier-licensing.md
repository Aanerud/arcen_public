# ADR 0006: Offline Pier Licensing

**Status:** Withdrawn.

## Context

This ADR recorded the former commercial offline Pier licensing design: a
first-party signed, machine-bound local license; protected clock state; host
maintenance commands; and a capacity-one admission lease.

## Decision

Withdraw the licensing decision. Arcen is now AGPL-3.0 free software, so the
commercial licensing and enforcement system was removed. The physical
capacity-one session admission and bounded direct-reconnect hold remain product
constraints and now live in host session-admission code instead of licensing
code.

## Consequences

- Pier startup no longer depends on a local license, key ring, or clock-state
  journal.
- License issuance, installation, validation, replacement, and enforcement
  commands are removed.
- The historical record remains here so the withdrawn design is not mistaken
  for current architecture.
