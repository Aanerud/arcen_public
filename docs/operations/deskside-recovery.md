# Deskside recovery

Deskside is disabled by default and not released yet.

## Recovery

On Windows, stop new sessions and run the existing `arcen-pier restore-display`
maintenance command as LocalSystem when the protected
`display-recovery.json` remains. Do not weaken the SYSTEM/Administrators ACL,
follow reparse points, or delete a failed journal before topology ownership is
adjudicated. Hooks are process-owned and disappear with the authenticated agent.
A journal left by a 0.20.4 or 0.20.5 session on a GeForce, whose only failure
was NVIDIA refusing the EDID purge, needs no manual step after upgrading: see
[the GeForce guide](windows-geforce-virtual-display.md#earlier-releases-0204-and-0205).

Desk privacy is on by default on every Pier (`privacy.desk`). On Windows its
default form needs no recovery of its own: the desk's keyboard and mouse
hooks end with the session agent, and the display lease and its journal are
the ordinary display recovery. The Windows agent proves its keyboard hook
every two seconds with an injected canary key. Windows calls no low-level
keyboard hook while a program in the session that reads raw keyboard
input, and whose own low-level keyboard hook is the newest, has its window
in front, so the hook cannot keep the desk's keyboard out then. The canary misses and
the Pier ends and locks the session (`deskside hook canary deadline
expired` in the agent log). The Windows Deck no longer does this (after
0.20.4); another such program in the session still ends it. On Linux a joined console's blanking is the
console privacy journal (`/run/arcen/console-privacy.json`, ADR 0025 §4).

On macOS (`privacy.desk`, formerly `platform.desktop.deskside.enabled`) there is nothing to
recover by hand: the desk's input filter and dark displays belong to the
signed-in user's session helper, and the window server removes the filter and
restores the displays' colour tables when that process exits. If a desk stays
dark, end the helper (`launchctl kickstart -k gui/<uid>/pier.arcen.tech.agent`
or sign out); `arcen-pier-macos probe-desk-privacy` run in that session shows
whether any display still reads dark.

On Linux, stop Pier and preserve `/run/arcen/deskside-recovery.json` with root
ownership, mode 0600, and a mode-0700 `/run/arcen` parent. Restarting Pier
replays the bounded xrandr/DPMS snapshot before listener readiness. Do not run
the console plan against the dedicated capture DISPLAY or delete a failed
journal until the physical console is manually usable.

Configuration intake requires fresh machine-local hash candidates. Windows
`diagnose-host --json` exposes hash-only SMBIOS, capture, and physical-output
facts. Linux operators compute the normalized DMI/chassis pin described in the
host architecture and configure the expected active local console UID. Never
copy these pins between hosts.

## Limits

Deskside does not cover SAS/secure desktop, kernel HID, pen/tablet, unsupported
connector/input classes, or isolation from a hostile process already running on
the Windows interactive desktop.
