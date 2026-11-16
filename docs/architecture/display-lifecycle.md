# Display lifecycle: what a session leaves behind

A Pier changes the host's displays to serve a Deck. When the session ends the
host must be usable by whoever sits at it: every display Arcen created is gone,
every display Arcen changed is back as it was, and the windows are on a display
the local user can see. The policy is shared
([`arcen_outputs::display_lease`](../../shared/outputs/src/display_lease.rs));
each Pier keeps only the operating-system calls.

## When the displays are returned

| Event | Action | Why |
| --- | --- | --- |
| Transport lost, restore lease still open (`auth.reconnect_window_secs`) | **Hold** | A reconnect must find the same layout; a network blip never tears down. The timing is `arcen_session::direct_reconnect` (`hold_restore_leases`). |
| Restore lease expired | Restore now | Nobody came back. |
| Explicit disconnect, Pier shutting the session | Restore now | The user is done. |
| The owner of the changes died (agent crash, Pier killed) | Restore now | The out-of-process watchdog replays the journal. |
| Pier started, console changed hands, and no session is live | Recover a pending journal | An unclean exit (sign-out killing agent and watchdog, power loss) left the journal behind. |
| Pier started or console changed while a session is live | Nothing | The live session owns its journal. |

`display_lease_action` encodes this table.

## What is returned

| Display | At the end of the session |
| --- | --- |
| Created by Arcen (IddCx monitor, NVIDIA headless EDID, `CGVirtualDisplay`, private Xorg output, Mutter virtual monitor) | Removed. |
| Present before the session and changed by Arcen (deactivated by isolation, made primary, moved, re-moded) | Restored: active, primary, position and mode as before. A mode the display no longer offers is tolerated; a topology difference (active set, primary, an Arcen display left) is not. |
| Plugged in during the session | Never touched. |
| Unplugged during the session | Nothing to restore; the remaining displays are restored without it. |

A third-party virtual display driver (for example
[Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)),
a dummy dongle and a monitor are all *host* displays: Arcen never removes or
disables them. `plan_teardown` classifies, `verify_restore` judges the result.

## Windows that a session stranded

Isolating a session display makes Windows gather every window there. When the
topology comes back Windows re-activates the other monitors but does **not**
move windows back onto a re-activated path. With a virtual display or a dummy
dongle as the session display the local user sees an empty monitor and every
window on a screen nobody can look at — the defect reported on 2026-11-15.

Decision: **the Pier moves them** (`plan_window_repatriation`), because Windows
will not, and because the Pier is the only party that knows which display the
session gathered windows on.

- A window on no active display moves to the primary.
- A window on a session display that was on a display the session did not
  serve goes back there (its own rectangle when it still fits), whichever
  display was primary — a primary VDD or dongle beside a monitor included. A
  move between two displays the session served was the remote user's and
  stays.
- A window that was on that display before the session stays: that was the
  user's choice.
- A window opened during the session (or whose display is gone) stays when the
  session display was the local user's primary (a dummy dongle alone, a
  monitor alone), and otherwise moves to the primary.
- The service never runs a console recovery while a session is live, and a
  session waits for a running one before it starts its agent: both act on the
  one journal.
- Cloaked windows (another virtual desktop), tool windows, owned windows and
  the shell's own windows are never moved. Minimised windows stay minimised;
  maximised windows are maximised on their new display. Moves never activate a
  normal window and never wait on a hung one (`WPF_ASYNCWINDOWPLACEMENT`).

The Windows Pier records the windows before it changes any display, writes the
record next to the display journal (`display-windows.json`), and moves windows
after a verified restore: from the session's lease, from the crash watchdog,
from a later session's recovery and from `restore-display`. Windows are only
moved on the user's desktop, never on the sign-in screen's; a process that
cannot move an elevated window (UIPI) leaves it and logs it.

## Per Pier

| | Linux | Windows | macOS |
| --- | --- | --- | --- |
| Displays Arcen creates | Private Xorg outputs, Mutter `RecordVirtual` monitors | NVIDIA headless EDIDs; IddCx monitors (dormant) | `CGVirtualDisplay` |
| Host displays Arcen changes | None (deskside xrandr blanking has its own restore) | Isolation, primary, position, mode, UI scale, HDR | None (virtual display origins only, `kCGConfigureForSession`) |
| Removal | With the desktop (idle timeout or sign-out) | Journal restore | Child process exit (pipe closed) |
| Unclean exit | Processes; outputs go with their server | Watchdog; console recovery at start and console change | Child exits on the closed pipe |
| Stranded windows | Die with the desktop | Moved back (`window_layout`) | macOS moves them off a departed display |

The differences are earned (ledger PD-032): only the Windows Pier attaches to a
console session and changes displays it did not create, and only Windows leaves
windows on a re-activated path. The macOS Pier has no direct resume yet
(GAP-003), so a transport loss there removes its virtual displays at once.

## Research

- [`SetDisplayConfig`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setdisplayconfig):
  `SDC_USE_SUPPLIED_DISPLAY_CONFIG` without `SDC_ALLOW_CHANGES` applies a
  snapshot exactly or fails; with it Windows may adjust modes; it is refused
  from session 0.
- [`IddCxMonitorDeparture`](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx-objects)
  destroys a monitor; monitors do not die with a control handle unless the
  driver ties them to file cleanup.
- Sunshine's display-device library persists the initial state and reverts on
  stream end and on the next start after a crash
  ([configuration](https://docs.lizardbyte.dev/projects/sunshine/v2025.921.42242/md_docs_2configuration.html)).
  Apollo removes its virtual display when the app ends and relies on Windows
  for the windows. Learned from, not copied.
- [`SetWindowPlacement`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowplacement)
  takes workspace coordinates for windows without `WS_EX_TOOLWINDOW`.
