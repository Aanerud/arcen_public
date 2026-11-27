# Deskside recovery

Desk privacy (`privacy.desk`) is on by default on every Pier: while a Deck is
attached, the desk's monitors are dark and its keyboard and mouse are blocked.
This page says what to do when a desk stays dark or blocked after a session.

Pinned deskside (`platform.deskside` on Linux, `platform.desktop.deskside` on
Windows) adds bare-metal and monitor pins to desk privacy. It is off by
default and not yet a released feature.

## Windows

Desk privacy needs no recovery of its own on Windows. The desk's keyboard and
mouse hooks belong to the session agent and end with it; the display lease and
its journal are the ordinary display recovery.

With `privacy.desk = "required"`, the desk stays dark, with its keyboard and
mouse blocked, after a Deck leaves a session that will neither lock nor
disconnect from the console; the Pier log says `privacy.desk is required and
the session will not lock`. Check that the session's lock screen is not
turned off by policy (the Pier asks for the lock every 30 s and gives the desk
back once the session is locked), or sign the user out. At the desk,
Ctrl+Alt+Delete still reaches Windows: choose Lock (the first entry), and the
desk gets its monitors back on the lock screen, where signing in is a normal
unlock. Stopping the Pier
instead hands the session to its session guard, which locks it once the
displays are back.

When the protected `display-recovery.json` remains after a session, the
next session first applies it. If it cannot be applied, the Pier sets it
aside untouched as `display-recovery.unrestorable.<time>.json`, logs
`display_restore_pending`, tells the Deck ("The displays could not be put
back after an earlier session."), and serves the session from the displays as
they are, with a journal of its own:

1. Connect and set the displays as they should be in Windows display
   settings, or replay the set-aside journal on the desktop with
   `arcen-pier restore-display --journal <file>` as LocalSystem.
2. `arcen-pier diagnose displays`, run on the desktop, shows the displays as
   the Pier reads them.
3. `arcen-pier restore-display --set-aside` sets a pending journal aside by
   hand; it works over SSH.
4. Do not weaken the journal's SYSTEM/Administrators ACL, follow reparse
   points, or delete a set-aside journal: it is the only record of what the
   stranded session changed.

After every session the Pier reads every display again and logs
`the displays are back exactly as they were before the session`, or
`display_restore_incomplete` with each display and value that differs.

A journal left on a GeForce by a 0.20.4 or 0.20.5 session, whose only failure
was the driver refusing the EDID purge, needs no manual step after upgrading:
see [the GeForce guide](windows-geforce-virtual-display.md#earlier-releases-0204-and-0205).

The agent proves its keyboard hook every two seconds with an injected canary
key. Windows skips low-level keyboard hooks while a program that reads raw
keyboard input, and whose own low-level hook is the newest, has its window in
front. The hook then cannot keep the desk's keyboard out, the canary misses,
and the Pier ends and locks the session; the agent log says
`deskside hook canary deadline expired`. The Windows Deck does not cause this;
another such program in the session still can.

## macOS

There is nothing to recover by hand. The desk's input filter and dark
displays belong to the signed-in user's session helper; when that process
exits, the window server removes the filter and restores each display's
colour table.

If a desk stays dark, end the helper
(`launchctl kickstart -k gui/<uid>/pier.arcen.tech.agent`) or sign out.
`arcen-pier-macos probe-desk-privacy`, run in that session, shows whether any
display still reads dark.

When a Deck leaves, the Pier waits for its virtual displays to go, puts a
display macOS moved back in its place, and logs `display_restore_incomplete`
for anything it could not put back. `arcen-pier-macos diagnose displays`,
run in the signed-in user's session, prints the displays as the Pier reads
them.

With `privacy.desk = "required"`, the desk also stays dark, with its input
blocked, after a Deck leaves a session that will not lock; the agent log says
`the session would not lock after its Deck left`. Turn the session's lock
screen back on (the Pier asks for the lock every 30 s and gives the desk back
once it is locked), or sign the user out. Ending the helper instead hands the
desk to its lock guard, which tries to lock it for a minute and then gives it
back, unlocked if it still will not lock.

The key was `platform.desktop.deskside.enabled` before `privacy.desk`; see the
[configuration reference](config-reference.md).

## Linux

A joined console's blanking is recorded in the console privacy journal
(`/run/arcen/console-privacy.json`;
[ADR 0025 §4](../adr/0025-arcen-joins-the-users-session.md)).

A joined X11 console's layout journal (`/run/arcen/console-layout.json`)
also holds every output as RandR read it before the Deck's layout. When the
Deck leaves, the Pier puts the layout back and reads the outputs again. If
they differ, it logs `display_restore_incomplete` with each output and value
and keeps the journal as the record: the next Deck's layout of that console
is put back to it, not to what was left. `arcen-pier diagnose displays`, run
as root, prints the console's outputs as the Pier reads them. Another
product's desktop keeps the Deck's layout when the Deck leaves, as that
product keeps its own; its layout before the Deck is recorded in
`/run/arcen/console-layout-before-deck.json`, not put back.

With pinned deskside, stop the Pier and keep
`/run/arcen/deskside-recovery.json` owned by root, mode 0600, in a mode-0700
`/run/arcen`. Restarting the Pier replays the bounded xrandr/DPMS snapshot
before it listens. Do not run the console plan against the dedicated capture
`DISPLAY`, and do not delete a failed journal until the physical console is
usable again.

## Pinned deskside configuration

Pins come from the machine itself and are never copied between hosts.

- **Windows:** `diagnose-host --json` prints hash-only SMBIOS, capture and
  physical-output facts to pin.
- **Linux:** compute the normalised DMI/chassis pin described in the host
  architecture, and configure the UID expected on the local console.

## Limits

Deskside does not cover the secure desktop (Ctrl+Alt+Del), kernel HID,
pens and tablets, unsupported connector or input classes, or a hostile process
already running on the Windows interactive desktop.
