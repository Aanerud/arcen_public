# Arcen Deck settings

Deck stores user configuration under `~/Library/Application Support/Arcen` on macOS, or
`ARCEN_CONFIG_DIR` when overridden.

## Saved connections

`connections.json` version 2 contains stable saved-connection IDs and a versioned settings
container per connection:

A clean installation creates no saved connections and contains no Arcen lab
hostnames or addresses. The first screen shows **No saved connections yet**
until the user adds a Pier.

```json
{
  "version": 2,
  "last_selected_connection_id": "connection-…",
  "connections": [{
    "id": "connection-…",
    "kind": "direct",
    "name": "pier-windows.example.internal",
    "host": "pier-windows.example.internal",
    "port": 18444,
    "transport": "quic",
    "security_mode": "medium",
    "settings": {
      "version": 1,
      "remembered_username": "admin"
    }
  }]
}
```

Host matching trims whitespace, lowercases DNS names, removes a terminal DNS dot, canonicalizes
IP literals, and includes port and transport. The security-mode discriminator prevents ambiguous
quick-connect matches. Explicit saved-connection IDs keep edit, selection, and deletion stable.
Unknown/future document, connection, and settings fields are preserved. Unsupported future
connection transports are retained without presenting an unsafe approximation in the UI.

Only usernames may be remembered. Passwords, authentication submissions, resume grants, and
other credentials are memory-only and are never fields in this file.

Deck updates the exact saved connection's username when a locally valid credential submission is
queued, not only after the host finishes authentication and session setup. Host-side console or
display failure therefore does not discard what the user submitted. Empty usernames, values with
control characters, and values longer than 255 UTF-8 bytes do not replace the prior saved value.
When a host disclaimer clears deferred credentials, accepting it reloads only that saved
connection's username; deferred secrets are never reused.

## Migration

Version-1 connection entries without IDs, ports, transports, or settings remain loadable. Deck
derives a stable migration ID and defaults omitted ports and transport to the existing connection
kind policy. A legacy `remembered_username` file, `settings.json` property, or old v2 fallback is
attached only to an identifiable last-selected connection. If none is identifiable, it is
discarded. Migration always clears the global source and leaves no global fallback.

## Global settings

`settings.json` continues to own USB, logging, security, performance, display, cursor, clipboard,
and HiDPI behavior. Display mode controls stream sizing: Windowed follows the app window, while
Primary display only pins to the primary display. Match my layout requests every active display,
up to four, and opens one native fullscreen Space per display. It preserves the primary display,
relative arrangement, exact presentation size, and rotation. Multi-monitor requires macOS
"Displays have separate Spaces" and standard fullscreen with Notch area off. If the host cannot
apply or encode the complete layout, the connection fails with guidance to choose Primary display
only; it never silently serves a subset. Those pinned modes request each display's
**fullscreen presentation size** — its logical
size minus the macOS safe-area insets — not its raw panel size, so the remote desktop lands 1:1
with no scaling. On a notched Mac the safe area is smaller than the panel (a 14" MacBook Pro at
1800x1169 presents 1800x1130), and pinning to the panel size made the viewer aspect-fit the
stream into the shorter viewport: a downscale plus letterbox bars. `fullscreen_uses_notch_area`
(Settings → Displays → Notch area) lets the user take the whole panel instead: macOS gives a
*standard* fullscreen window only the safe area and offers no public API to change that, so Arcen
hides the menu bar and Dock and places a borderless window over the full screen frame. Both
settings are 1:1; the choice is whether the notch strip is black or shows remote pixels that the
notch itself partly covers. In notch mode the menu bar and Dock auto-hide rather than disappear,
so pushing the pointer to the top edge still drops the menu bar down over the session — otherwise
there would be no way to reach Connection → Disconnect. `remote_ui_scale_percent` (0 = automatic)
sets the Windows display-scaling percentage the remote desktop comes up at, by advertising a
synthetic EDID physical size of `pixels * 25.4 / (96 * percent / 100)` millimetres; automatic
reports the panel's real measurements. This changes the remote desktop's own layout, never the
stream's 1:1 pixel mapping. The advanced numbered display controls let the user require 4:4:4 for
selected displays; unselected displays explicitly permit bandwidth-optimized 4:2:0. This is a
visual-quality intent only: Pier remains authoritative for allowed
GPUs, hardware/software encoder assignment, and admission. When HiDPI streaming is on,
pinned modes request the backing-pixel presentation size so Retina panels get true physical 1:1,
and the requested refresh is capped at 60 Hz so the synthesised mode stays inside the EDID 1.4
pixel-clock ceiling.
The per-connection settings container is
intentionally extensible so selected global options can move there in a future reviewed schema
version without another connection-file redesign.

Fresh-install video defaults are **Standard performance** (30 fps ceiling) and
**Standard (Adaptive) colour fidelity** (automatic AV1/HEVC/H.264 selection,
4:2:0 8-bit limited range). No codec is pinned by Deck.

`tablet_mode_requested` stores the per-connection tablet mode:
`local_termination` (default), `wacom_usb_bridge`, or `disabled_mouse_compat`.
Deck still persists `tablet_input_enabled` for legacy readers, but it is derived
from mode (`true` only for `local_termination`). Mode changes require reconnect.
The Add/Edit Connection screen owns the saved connection's value; the Advanced
setting updates the active or last-selected saved connection.

- **Tablet support** (`local_termination`): recommended for WAN and ordinary
  remote-work connections. It activates when Deck detects the tablet and both
  peers prove input-v3 pen support. The Wacom driver is required on this Mac
  only; the tablet remains usable locally.
- **Native tablet mode** (`wacom_usb_bridge`): explicit LAN/KVM opt-in for future
  host-driver communication. It requires Wacom drivers on both this Mac and the
  host plus a complete USB bridge backend. Current hosts reject it with an
  explicit reason and keep the active mode at mouse compatibility; they never
  substitute Tablet support.
- **Mouse compatibility only** (`disabled_mouse_compat`): disables tablet
  redirection and keeps ordinary mouse behavior, with no tablet dependencies.

Deck resolves production paths through one config repository. Tests inject explicit
repository-local paths; their default repository is disabled, so parallel tests and panics cannot
fall through to `~/Library/Application Support/Arcen`.

Connection mutations are cross-process transactions guarded by the adjacent
`connections.json.lock` file. Each transaction reloads the current document and changes only the
target stable ID (selection, username, add, edit, or delete). A missing or concurrently edited
entity is reported instead of being recreated from a stale UI snapshot. Full snapshots, used only
for migration/testing, carry a content fingerprint and fail if the file changed. Lock acquisition
uses nonblocking attempts with a strict 100 ms deadline; if another Deck instance remains active,
the operation reports that the saved connections are busy instead of freezing the UI.

Both JSON files use the same crash-safe writer: a unique same-directory file is created without
following an existing path, fully written and synced, atomically renamed, and followed by a parent
directory sync. Existing permissions are retained. An existing malformed or unreadable document
is reported and preserved; starter connections are used only when `connections.json` is absent.
