# Timezone Redirection

**Status (2026-10-05):** implemented on macOS Deck and all active Piers.
The feature is opt-in and defaults off on both hosts. It is a convenience
feature: failure warns and streaming continues.

## Wire authority and compatibility

The Deck sends one optional IANA identifier in `AuthResponse.timezone`.
Because the hosts must decide the desktop environment before session creation,
this pre-session value is authoritative. The later optional
`ClientHello.timezone` is consistency-only: a mismatch is logged and never
changes the authenticated decision or an existing desktop.

Both fields use serde defaults and omit `None`, so feature-off and old-client
JSON is unchanged. The additions are backward-compatible protocol-v3 fields;
the protocol version remains v3. Windows identifiers never cross the wire.

## Shared and client behavior

The default `arcen-session` surface validates a bounded, path-safe IANA syntax
and models restore leases independently of any OS. Semantic existence belongs
to each host. A lease records a bounded owner, resource, original and target
SHA-256 fingerprints, and an explicit phase. Apply and restore transitions are
idempotent for at-least-once recovery; an observed state matching neither
original nor target becomes a conflict and is held for operator action.

On macOS, Deck captures one current IANA name for the connection attempt and
uses that value in both messages. The portable non-macOS implementation returns
`None`. Deck does not translate to a Windows time-zone name.

## Windows Pier: privileged system-wide scope

Windows redirection runs only in the LocalSystem broker. It changes the
machine's time zone system-wide, so enabling it is a privileged operational
risk for every user and service on that host, not a per-application setting.
It makes no universal-application or certification claim.

The adapter:

1. Validates the IANA identifier and maps it through the exact approved CLDR
   48.2 `windowsZones.xml` build input. Parsing is deterministic at build time;
   there is no runtime fetch or committed generated Rust source.
2. Resolves the mapped Windows name through the OS and captures the complete
   `DYNAMIC_TIME_ZONE_INFORMATION` original and target snapshots.
3. Enables `SeTimeZonePrivilege` only for the scoped operation.
4. Writes the bounded
   `%ProgramData%\Arcen\recovery\timezone-recovery.json` journal before mutation,
   using atomic replacement, synchronized writes, and persisted phase changes.
   Installation protects this dedicated sibling directory for SYSTEM and
   Administrators only; runtime access rejects reparse points in existing path
   components. Pier validates this boundary but does not install its ACL.
5. Holds the lease through the user-session agent, restores it before releasing
   the machine permit, and removes the journal only after confirmed restore.

Helper startup reconciliation handles an interrupted owner.
Current state equal to the target restores the original; current state equal to
the original removes completed recovery state. If it matches neither snapshot,
the journal is retained in conflict/hold state rather than overwriting an
operator or third-party change. The support bundle includes only safe,
redacted recovery metadata.

Disabled, absent, invalid, unmapped, already-current, or privilege-denied
requests produce nonfatal diagnostics and continue streaming. Journal
ambiguity disables timezone redirection but does not prevent Pier from serving
sessions.

Configuration: `redirection.timezone` in unified `pier.json`; omitted or
`false` means disabled.


## macOS Pier: GUI-session `TZ` scope

macOS serves an existing Aqua desktop, so Arcen does not create a new process
tree the way Linux does. The qualified scope is therefore the served user's GUI
launchd domain, not the machine timezone. The existing Agent Helper validates
the authenticated Deck IANA identifier against `/var/db/timezone/zoneinfo`,
rejecting malformed names, `posix`/`right` alternates, non-files, and symlink
escapes through the shared zoneinfo validator. It then sets
`ARCEN_SESSION_TZ=<IANA>` followed by `TZ=<IANA>` in that user's GUI launchd
domain from inside the LaunchAgent.

This is intentionally session-scoped and unprivileged. Applications launched
after the setting is applied inherit the Deck timezone. The menu bar clock, the
system timezone, and applications already running before the session keep the
Mac's local timezone or their cached environment. No root LaunchDaemon, socket,
or machine-wide recovery journal is involved.

The `ARCEN_SESSION_TZ` sentinel is the only state. On session end, SIGTERM, or
the next agent start after a crash, the Agent Helper checks the sentinel: if
`TZ` still equals it, both variables are unset; if another writer changed `TZ`,
only the sentinel is removed. A pre-existing user `TZ` with no sentinel is
reported as user-defined and Arcen does not redirect. If an Agent Helper is
SIGKILLed before it can restore, `TZ` can remain in that GUI session until the
user logs out; it never survives that launchd domain. Invalid, absent,
unsupported, already-current, or failed launchctl requests warn and continue
streaming.

The package still removes the short-lived rc5 root timezone helper on upgrade
and uninstall. If its old system-timezone journal is present and the Mac is
still on the Deck target zone, the root install script restores the recorded
original once before deleting the legacy plist/socket directory.

## Linux Pier: authenticated process-tree scope

Linux redirection is available only for authenticated PAM dedicated persistent
desktops. The adapter validates the shared syntax, then canonicalizes the entry
under a configurable trusted zoneinfo root (default `/usr/share/zoneinfo`). It
rejects escapes, non-files, and entries in the alternate `posix` or `right`
trees.

Before opening the PAM session, the launcher adds only `TZ=<IANA>` to the PAM
environment. That trusted `SessionEnvironment` is propagated to the session
agent, GNOME and agent children, and the user activation environment through
`dbus-update-activation-environment --systemd`. It never modifies
`/etc/localtime`, invokes `timedatectl`, or changes machine-wide systemd
environment.

The desktop owns its in-memory lease across disconnect/reconnect. A reconnect
mismatch warns and retains the running desktop's timezone; final process
teardown completes the lease. No durable Linux timezone journal is written.
The activation update belongs only to the authenticated user's D-Bus activation
environment and user manager. The dedicated PAM desktop owns this per-user
state, with PAM/logind teardown and user-manager lifecycle as the restoration
boundary. Because the D-Bus update API cannot exactly restore a previously
absent variable, teardown does not set an empty `TZ`. Lingering user managers
and applications can cache or retain the value and require an operator-managed
user-manager or application restart. No-auth mode is inert.

Configuration: `--timezone-redirection` enables the feature,
`--no-timezone-redirection` disables it, and `--zoneinfo-root` selects the
trusted database. The effective default is disabled.

The exact direct-QUIC detach/resume/drain ordering that holds these Windows and
Linux leases without reapplying them is in
[`session-auto-reconnect.md`](session-auto-reconnect.md).

## Failure and test boundary

Timezone handling must not block authentication or streaming. Invalid,
missing, unsupported, privilege-denied, reconciliation-conflicted, and
consistency-mismatched inputs are warning paths; each host retains its safe
existing scope.

Pure tests cover protocol-v3 JSON compatibility, IANA bounds, lease retries and
conflicts, CLDR mapping generation, host policy decisions, journal phases, and
Linux path validation through injected or temporary fixtures. Target-platform
tests cover adapters without claiming every OS database, DST transition,
application cache, privilege policy, crash timing, or multi-user deployment.
Those behaviors require target-OS and operational acceptance testing before
enabling the feature on a workstation.
