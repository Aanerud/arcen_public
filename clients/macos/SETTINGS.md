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
      "version": 2,
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

0.20.7 writes connection settings version 2 and `settings.json` version 2 (`"version": 2`; a
file without it is version 1). Reading an older file changes nothing on disk; the next save
writes the migrated values and keeps every key it does not know:

- The tablet mode is one application-wide setting (`tablet_mode_requested` in `settings.json`);
  a version-1 file's value is reset to `local_termination` (Tablet support), once. A
  connection's own `tablet_mode_requested` is dropped.
- The Option keys go back to Automatic, once: `option_keys` in a version-1 file becomes
  `automatic`; a connection's `left_option_altgr` is dropped.
- `typing_mode` `characters` ("Always characters") is read as `auto` (Characters), in any
  version: it is no longer a setting.
- `keyboard_capture` `fullscreen` ("the remote computer when fullscreen") is read as `host`
  with `capture_mac_system_shortcuts` on, in any version: the system-shortcuts switch on.
- A connection's `keyboard_mode` becomes its own `typing_mode`: `characters` becomes `auto`
  (Characters), `physical` follows Settings.

Paste as Keystrokes tuning, `cursor_preference` and every other value keep their values.

## Where each setting is shown

0.20.7 lays the pages out the same on both Decks (shared
`arcen_deck_core::screens::{settings, input_choices, home, list_picker}`):
each page in one centred column, one sentence-case heading per group, every
choice from a short list in the shared list picker (the value in force and
one line on what it does; the choices open in place), a tick box for each
switch. The explanations are in the [user guide](../../docs/user-guide.md).

There is no appearance setting: the Deck follows the operating system's
light or dark appearance, live, from the two palettes in
`arcen_deck_core::screens::theme`. Nothing about it is stored.

| Page | Group | Keys |
| --- | --- | --- |
| Settings › Input | Keyboard: **System shortcuts go to the remote computer** | `capture_mac_system_shortcuts` and `keyboard_capture` (on: `true` and `host`; off: `false`). Off by default on a Mac, as before; the Windows Deck's switch is `keyboard_capture` (`host` or `this_computer`), on by default. "Needs Accessibility permission." shows only while it is on without the permission. |
| | Typing: **Characters** (Recommended), **Keys only** | `typing_mode` (`auto`, `keys`); the line beside it counts the connections with their own `typing_mode`. |
| | Clipboard: **Share the clipboard with remote computers** | `clipboard_enabled`; the line under it counts the connections with their own `clipboard_enabled`. |
| Settings › Tablet | **Tablet support**, **Native tablet**, **Mouse only**; pen buttons under Tablet support | `tablet_mode_requested`, `pen_lower_button`/`pen_upper_button` (one barrel button on the Windows Deck) |
| Settings › Advanced › Input | Keyboard switch; Paste as Keystrokes: **Speed**, **Ask before pasting more than** | `keyboard_switch`, `paste_chars_per_second`, `paste_confirm_above` |
| New and Edit connection › Connection (left column) | **Address** (`host`, `host:port` or `[v6]:port`; 18444 when no port is given), **Name** (optional; empty saves as the address), **Username** (optional) | the connection's `host`, `port`, `name` and remembered username |
| New and Edit connection › On this machine (right column, always shown; under Connection in a narrow window) | **⌘ key** (Windows Deck: **Command key on a Mac host**), **Option key** (Mac only), **Typing**, **Displays**, each a list picker | the connection's `keyboard_shortcuts`, `option_keys`, `typing_mode`, `displays_mode`; following Settings reads "App default (Characters)" |

`cursor_preference` is no longer shown either: the cursor is automatic, and
the stored value is only the request an older host still gets.
`option_key_mode` (global `option_keys`) is no longer shown in Settings: it
stays Automatic, and a connection can keep its own Option key. A connection's
`clipboard_enabled` is set only by a connection profile. When the host's hello
advertises its clipboard policy as `disabled`, the session says "Clipboard is
turned off by this host." (the keyboard indicator's menu on a Mac); it is never
a setting.

## Global settings

Every connection asks for the automatic cursor (`cursor_automatic_v1`): a host that knows it
(the macOS Pier from 0.20.7) decides (cursor images: the Deck draws them; none, as on the macOS
Pier: the host draws the pointer into the video). `cursor_preference` (`local` by default, or
`host`) is never migrated: it is sent unchanged as the request every other host serves. The Deck hides its own
pointer whenever the host reports that it draws the pointer (`active` Host).

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
1800x1169 presents 1800x1130 — the menu bar, 39 pt, is one point taller than the 38 pt notch
inset AppKit reports, and the Deck uses the larger, so a Retina stream is exactly 3600x2260),
and pinning to the panel size made the viewer aspect-fit the
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
The per-connection settings container holds a connection's own values for the settings that
depend on the host, under the same keys as `settings.json` (`typing_mode`, `option_keys`,
`displays_mode`, `clipboard_enabled`; `keyboard_shortcuts` has no application-wide setting).
An absent key follows Settings. `clipboard_enabled` can only turn the clipboard off for that
connection: the user's consent in Settings is the ceiling. Unknown keys are kept.

Settings → Streaming → **Colour check…** (and Session → Colour Check… in a
session) opens the colour checklist ([guide](../../docs/operations/colour-check.md)).
Its one-click fixes write the same settings as this screen — the streaming
preset, and `hidpi_streaming` plus the primary display's `hidpi_display_mask`
bit for Retina-resolution streaming — and then reconnect where the host needs a
new connection to serve them. They never change a host setting. Saved reports
and test-pattern PNGs go to `~/Documents/Arcen/Colour Checks/`.

Fresh-install video defaults are **Standard performance** (30 fps ceiling) and
**Standard (Adaptive) colour fidelity** (automatic AV1/HEVC/H.264 selection,
4:2:0 8-bit limited range). No codec is pinned by Deck.

`tablet_mode_requested` in `settings.json` stores the tablet mode, one choice for every
connection: `local_termination` (default), `wacom_usb_bridge`, or `disabled_mouse_compat`.
`wacom_usb_bridge` is the 0.15 spelling of Native tablet, still written so an
older Deck can read the file; Deck also accepts the neutral
`usb_tablet_bridge` on load. Neither spelling is tied to a tablet maker.
Deck still persists `tablet_input_enabled` for legacy readers, but it is derived
from mode (`true` only for `local_termination`). Mode changes require reconnect.

- **Tablet support** (`local_termination`): recommended for WAN and ordinary
  remote-work connections. It activates when both peers prove input-v3 pen
  support; the Deck offers it whether or not a tablet is attached when you
  connect, so a tablet of any make plugged in (or first used) later in the
  session works without reconnecting. The tablet's own driver, if its maker ships one, is needed on this
  Mac only; the tablet remains usable locally.
- **Native tablet** (`wacom_usb_bridge`): explicit LAN/KVM opt-in that
  bridges the physical tablet to the host over USB so the host's own driver
  for it takes over. It is offered for any device whose HID report descriptors
  classify as a pen tablet, and needs a host with a complete USB bridge backend
  (today only lab builds of the Linux Pier). Hosts without one reject it with
  an explicit reason and keep the active mode at mouse compatibility; they
  never substitute Tablet support.
- **Mouse only** (`disabled_mouse_compat`): disables tablet
  redirection and keeps ordinary mouse behavior, with no tablet dependencies.

`keyboard_shortcuts` stores what Command and Control are on the connection's
host: absent (Automatic — the host platform's own mapping, learnt from its
hello), `windows_app` (Command is the Windows key, Command+C/X/V/A/Z/F/W are
Control), `swap_command_control` (Command is Control, Control the Windows
key), or `command_is_control` (both are Control). A Mac host keeps Command as
Command whatever is stored. It is absent at its default, so connections saved
before it are unchanged and read as Automatic. They replace the Deck's old in-memory Cmd→Ctrl switch,
which every connection had on and which was never stored. See the README,
"Your Mac keyboard on the host".

`typing_mode` stores the connection's own Typing: absent (follow the global
Typing setting), `auto` (Characters) or `keys` (Keys only). The Connection
menu's **Keyboard: Physical Keys** and **Keyboard: Characters** (every typed
character as the Mac typed it; shortcuts, navigation and function keys stay
keys), menu items without a shortcut, override it for the session;
that choice is never stored, and lasts across reconnects to the same endpoint
only.

Global `settings.json` keys for **Paste as Keystrokes**:
`paste_chars_per_second` (default 25, clamped to 5–500; the Settings page
offers 10, 25, 50, 100 and 250) and `paste_confirm_above` (default 2000: longer
clipboard text asks before it is typed; clamped to 1–100,000, which is also
the most one paste types). The clipboard text itself is never stored or
logged.

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
