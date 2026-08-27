# Deskside recovery and physical acceptance

Deskside is disabled by default and is implementation-complete but not released:
the Windows and Linux physical matrices are unrun mandatory gates. VM results
prove refusal and orchestration only.

## Recovery

On Windows, stop new sessions and run the existing `arcen-pier restore-display`
maintenance command as LocalSystem when the protected
`display-recovery.json` remains. Do not weaken the SYSTEM/Administrators ACL,
follow reparse points, or delete a failed journal before topology ownership is
adjudicated. Hooks are process-owned and disappear with the authenticated agent.

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

## Mandatory release gates

Windows must prove physical/injected keyboard and mouse separation, every pinned
monitor blank/restore, distinct capture output, reconnect hold/resume/expiry,
hot-plug, sleep/resume, driver reset, and agent/broker/service crash recovery.

Linux must prove complete physical evdev enumeration and grabs without affecting
Arcen uinput, every pinned DRM output blank/restore while dedicated Xorg capture
continues, reconnect hold/resume/expiry, hot-plug, sleep/resume, GPU reset, and
launcher/broker/service crash recovery.

Neither matrix may claim SAS/secure desktop, kernel HID, pen/tablet, unsupported
connector/input classes, or isolation from a hostile process already running on
the Windows interactive desktop.
