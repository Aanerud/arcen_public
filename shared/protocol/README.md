# arcen-protocol

The wire format: the single source of truth for what a Pier and a Deck say to
each other. Host-agnostic: serde types, binary framing codecs, the session
state machine and the auth hash. No I/O.

## What lives here (reuse before adding)

- **Messages and negotiation** — `messages` (hello, results, capabilities;
  every addition negotiated), `fsm`, `auth`, `build_identity`.
- **Lanes and framing** — `lanes`, `wire`, `keys`.
- **Cursor** — `cursor` (shape and visibility updates) and `cursor_position`
  (`CursorPosition`, frame type `0x51`, `cursor_position_v1`: where the host
  pointer or a bridged pen is; `CursorPositionRelay` renumbers records for a
  split Pier). Pen and tablet positions use this record; do not add another.
- **Clipboard, multi-monitor, regions, refinement** — `clipboard`,
  `multi_monitor`, `region_input`, `refine`.

Consumers are registered in `consumers.json`.
