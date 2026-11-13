# arcen-input

Transport-independent keyboard, pointer and pen contracts shared by every Pier
and the Deck. No OS calls.

## What lives here (reuse before adding)

- **Pen position for Native tablet** — `tablet_pointer`: `PenSample` (what a
  platform adapter produces), `EvdevPenFrames` (evdev to samples),
  `PenPointerLead` / `PenPointerHub` (whether the pen leads), `PenMapping`,
  `x11_wacom_mapping` (remap check), `map_to_output` (scaling). One path for
  every Pier (ADR 0023): a new Pier adds an adapter, not a new tracker.
- **Host pointer positions and the Deck's pen dot** — `host_pointer`:
  `HostPointerFeed`, `HostPointerReporter`, `HostPointerLayout` (Pier side);
  `DeckHostPointer`, `PointerAuthority`, `led_pointer_mark` (Deck side).
- **Typed pen (Tablet support)** — `pen_axes`, `pen_edges`, `pen_forwarding`,
  `pen_emulation`, `seat_pen`, `hid_digitizer`, `hid_reports`.
- **Keyboard and text** — `keyboard` (policy, shortcut translation, scan
  codes), `text` (typed text strokes).
- **Cursor** — `cursor` (authority and shape rules); the wire record for
  positions is `arcen-protocol`'s `cursor_position`.
- **Regions and negotiation** — `region*`, `negotiation`, `lane_outbox`,
  `virtual_device`, `gestures`, `output_local`.

Who uses which module is registered in `consumers.json`
([`docs/architecture/shared-consumers.md`](../../docs/architecture/shared-consumers.md)).
