# Trust boundaries

Arcen treats Decks, Piers, network metadata and persisted configuration as
separate trust inputs. This page states each boundary, what enforces it, and
its known limits.

## Required separations

- The host's operating system authenticates the user: PAM on Linux and on a
  macOS Pier, the Windows logon path on Windows. Arcen keeps no user
  database.
- The Pier's validated TLS certificate (its SPKI) identifies the host to the
  Deck.
- Session authorization binds the verified user, host and policy decisions to
  one bounded session.
- Transport encryption protects the channel; it never replaces
  authorization.

Arcen has no licence enforcement ([ADR 0006](../adr/0006-offline-pier-licensing.md)).
The one-session-per-host gate is a limit of the host's resources, not an
entitlement.

## macOS Pier process boundary

The macOS Pier splits into a network service running as the unprivileged
`_arcen` account and a desktop agent running in each signed-in session as that
person. The two verify each other over a local socket. Only the service holds
the host key and the port; only the agent can capture or inject input. At the
login window the agent runs as root, which is the one open gap. The reasons,
the enforcing code and the installer prompts are in
[`macos-pier-process-model.md`](macos-pier-process-model.md).

## Linux Pier SELinux boundary

On an SELinux host the Linux Pier and its session launcher run as the
service domain `unconfined_service_t`; the session user's desktop runs in
the context `pam_selinux` assigns, applied only to the processes the
launcher starts as that user. The installer's one-rule `arcen` module
permits that transition; a user whose context the policy does not let the
launcher enter is refused, never run in the service's domain. See
[`linux-selinux.md`](linux-selinux.md).

## Linux remote desktops and polkit

A remote desktop's user must not hold the console's polkit rights. polkit's
systemd backend counts a subject as local when its logind session has a
seat (`sd_session_get_seat`) and never reads logind's `Remote` flag; a
process without a session is judged by its user's graphical session
(`sd_uid_get_display`). A local, active subject gets each action's
`allow_active` default — on stock systems power off, reboot, suspend,
NetworkManager network control, udisks2 mounting and more, without a
password. Every Linux X11 desktop is a logind session on `seat0` at VT 1
that the launcher activates, so without a rule its remote user, and every
session-less process of that user, would hold those rights. Headless
Wayland desktops are seatless.

The installer installs, and the Pier rewrites at every start,
`/etc/polkit-1/rules.d/05-arcen-remote-session.rules`
(`arcen_session::polkit_remote`). For a subject in a seated session that
logind records as remote (`REMOTE=1`, from `PAM_RHOST`, which every Arcen
desktop sets to the Deck's address), that an Arcen launcher opened
(`TTY=arcen`), or that sits on an Arcen desktop seat (`seat-arcen-pen-…`),
it returns each installed action's `allow_any` default and `AUTH_ADMIN` for
an action it does not know; an action whose `allow_any` is `YES` is left to
the later rules. A session record it cannot read counts as remote (polkitd
logs the first failure once, unless it runs with `--log-level=err` as on
Rocky/RHEL 10; the Pier's console self-test reports it either way). Only root can create logind sessions
(`CreateSession` is not open to other users on the system bus), so neither
marker can be minted by an unprivileged user. The rule runs before
distribution rules (`05-`), reads the logind record through `polkit.spawn`
(polkit's JavaScript sees only `seat`, `session`, `local` and `active`), and
names no session, so sign-ins never write it.

A process in no logind session (a systemd user service: GNOME 40 and later
run the shell and the settings daemons that way) reaches the rule with no
session and no seat, but `local` from its user's display session and
`active` when any of the user's sessions is active. Such a process may
belong to any of the user's sessions, so the rule counts it as remote while
any of them is an Arcen desktop or a seated remote session:
it resolves the user name to a uid with `id` (cached per name), reads
`SESSIONS=` from logind's user record (`/run/systemd/users/<uid>`) and
classifies each listed session as above (logind 252 on Rocky 9 leaves an
ended session in that list until the user's next session starts, so a
listed session missing from a successful `ls` of `/run/systemd/sessions`
is gone and skipped; a listing that fails skips nothing). Otherwise — only console sessions,
or ssh logins, which are seatless — polkit's own answer stands. A uid, user
record or session record it cannot read still counts as remote. The cost:
while a user has a separate Arcen desktop open (a dedicated or native
desktop beside their desk session), their desk session's user services get
the remote answers too; a joined console session (ADR 0025) is the user's
own session and has no such second session.

The user of an Arcen desktop (`TTY=arcen` or an Arcen seat, not merely
`REMOTE=1`) is granted, without a password, exactly the colord actions
GNOME's colour plugin calls for the desktop's monitors and the user's own
ICC profiles (`org.freedesktop.color-manager.create-device`,
`create-profile`, `delete-device`, `delete-profile`), and only for a subject
whose uid is the session's own (logind's `UID=`). colord does not check
who owns a device or profile it deletes, so, like a user at the desk, such
a user can delete any colour device or profile (colour settings only; LOW,
recorded). Every other action,
including colord's system-wide profile installation, device inhibition and
sensor locking, keeps its remote answer. A site rule numbered `00`–`04` can
still deny them.

A seated desktop is refused before any session is opened while the rule is
not installed (`polkit_rule_missing`). After its session is active and
before it is handed over, every desktop is proved: the Pier runs `pkcheck`
against a process in the session for a fixed set of actions stock systems
grant local users. A seated desktop is refused, and its processes killed at
once, on any grant or missing answer (`polkit_grants_remote_session`,
`polkit_unverified`); a seatless desktop only on a grant. At start the Pier
also asks polkit about a local console session, when one exists, and warns
when the console is refused a grant its defaults promise.

Known limits:

- polkit older than 0.106 has no JavaScript rules. There an X11 desktop is
  still served, with a `SECURITY:` error at start and on every sign-in
  (`polkit_no_rules`); refusing it instead would end X11 service there.
- **Later denies are bypassed.** Because the rule answers first, for a
  remote seated desktop a site rule numbered after `05-` (or a `.pkla`
  file) no longer applies to any action whose `allow_any` is `NO` or an
  authentication (`AUTH_SELF`, `AUTH_ADMIN`, `_KEEP` forms): neither a later
  grant (for example a `libvirt` group rule) nor a later deny or lockdown
  (`NO`, or `AUTH_ADMIN` where `allow_any` is `AUTH_SELF`). Site
  restrictions and grants for remote desktop users must be numbered
  `00`–`04`; `arcen-pier polkit-rule status` lists later site rules
  (`/etc/polkit-1/rules.d`) and `.pkla` files that look like restrictions. Actions whose `allow_any` is `YES` fall
  through to the later rules unchanged.
- **The proof window.** Between the session's activation on its seat and
  the `pkcheck` answer (the desktop's start-up, seconds) the session's own
  programs run with whatever polkit gives them. With the rule installed —
  required before launch — that is the remote answer; the window matters
  only when the installed rule is not effective, which the proof then
  detects.
- An action installed after the Pier last started is answered `AUTH_ADMIN`
  until the next start.
- The rule relies on logind's session record format and on polkitd being
  allowed to run `cat`; if it cannot read a record it treats the session as
  remote, which can only take rights from a console user, never give them to
  a remote one. The console self-test makes that visible.

## Linux desktop session services

A Linux desktop has no display manager, so the Pier's launcher and agent do
what one would. Each step is a trust boundary:

- **Login password to the keyring (headless Wayland only).** After PAM
  accepts the password, the root launcher keeps one zeroizing copy until it
  starts the session agent, writes it to the agent's standard input and
  closes the pipe. The agent, running as the session user, makes itself
  non-dumpable (no core dump, no ptrace by the user's other processes; its
  children are dumpable again at their own exec). It reads the password once
  from file descriptor 0 directly into one pre-allocated zeroizing buffer, so
  no stdin buffer keeps a copy, bounded at 8 KiB and 5 s, then points
  descriptor 0 at `/dev/null`. It writes the password to the standard input of
  `gnome-keyring-daemon --unlock`, which it starts on the session's private
  bus. Every copy is zeroized after use. The password is never in an
  argument, an environment variable, a file or a log. It ends up exactly
  where a GDM login puts it (`pam_gnome_keyring` hands it to the same
  user-owned daemon). If it does not open the login keyring, the keyring
  stays locked and applications ask for the password. A second desktop of a
  user who is also signed in at the console runs a second keyring daemon on
  the same keyring files; gnome-keyring re-reads keyring files that change.
- **Login password to the keyring (console sign-ins that start the
  session).** GDM's login screen taking a one-time token and Arcen's own
  login session on a host without a display manager never show the password
  to `pam_gnome_keyring`. For those two only, the root launcher keeps one
  zeroizing copy of the password it just proved until the session's agent
  is up, then writes it to the standard input of a short-lived helper
  started as the session user (`session-agent --login-keyring-unlock`,
  environment cleared, privileges dropped, uid 0 refused) and drops it.
  The helper reads at most 8 KiB, waits at most 30 s for the user's own
  keyring daemon, refuses a control socket in `/run/user/<uid>/keyring`
  that is not a socket owned by the user or whose peer is another uid, and
  sends the one `UNLOCK` request `pam_gnome_keyring` sends there. Only the
  result is logged. A joined existing session is never touched.
- **Login-shell environment (both providers).** The agent, as the session
  user and never as root, runs the user's login shell once
  (`$SHELL -l -c 'exec env -0'`). The shell must be listed in `/etc/shells`
  and must not be a no-login shell. The run is bounded to 5 s and 64 KiB,
  and its process group is killed afterwards. The NUL-separated output is
  filtered by `arcen_session::desktop_session::login_environment`: the
  display, runtime directory, buses, keyring, `SSH_*`, `LD_*`, `DBUS_*`,
  locale and time-zone variables Arcen owns never enter. Imported names are
  logged, never their values. A failure is one log line. Nor can it change
  how the compositor composes or which GPU and driver it uses: Mutter,
  Clutter, Cogl and GNOME Shell switches (such as `MUTTER_DEBUG_FORCE_HDR`)
  and `GSETTINGS_BACKEND` (which would bypass the pinned settings) are
  denied. EGL/GLX/Mesa/GBM driver and PRIME selection (`__EGL_*`, `__GLX_*`,
  `__NV_*`, `LIBGL_*`, `MESA_*`, `GBM_*`, `DRI_PRIME`, `EGL_PLATFORM`) is the
  user's choice for their applications: the session bus and the activation
  environment (D-Bus-activatable applications, user services) and autostart
  programs get it, but the compositor (and the Xwayland it starts) is
  started without it (`selects_gpu_or_driver`). A program GNOME Shell
  spawns directly inherits the shell's environment and so does not see it.
  The compositor's command removes
  `MUTTER_DEBUG_FORCE_HDR` unless the session is a forced-HDR one. Where the
  user's configuration lives (`XDG_CONFIG_HOME`, `XDG_CONFIG_DIRS`) is
  honoured: autostart, its conditions and the first-login marker are read
  from the session's own environment. On the dedicated Xorg desktop the
  imported variables also go to the user's bus and systemd user manager
  (`dbus-update-activation-environment --systemd`), as gnome-session uploads
  its environment on a GDM login.
- **Autostart (headless Wayland).** The user's and the system's XDG autostart
  programs run as on a GDM login, as the session user in the session's
  scope, and are stopped at teardown. Entries that act on the host rather
  than on the remote user's desktop never run. They are the settings
  daemon's time zone, radio, sharing, USBGuard and modem plugins, and
  virtual-machine console agents.
- **Host power.** The session manager the agent serves reports
  `SessionIsActive=false` and `CanShutdown=false` and never sets logind's
  `IdleHint`. A per-session dconf overlay pins the suspend and power-key
  actions to `nothing`. Deck input cannot produce a host power, sleep,
  suspend or wake key (pinned by a test). For every desktop, on either
  provider, the launcher holds a logind `sleep:idle` block inhibitor
  (`systemd-inhibit`) that goes away with the launcher's pipe. A host serving
  a remote desktop never suspends under it, except by a privileged request or
  a physical lid close.

## Direct-QUIC certificate boundary

Direct QUIC is the only live transport and listens on UDP 18444. The
dependency-light default of `arcen-transport` owns the common rustls/ring
posture and certificate/SPKI validation decisions. Product adapters own
protected PEM I/O and explicit reload; packaging helpers own optional SMB
issuance. The Pier process never invokes a helper, OpenSSL, a certificate
store, or a runtime issuer.

All PEM is untrusted until strict pre-listen validation succeeds. The leaf must
be current, non-CA, SAN-bearing, server-auth/digital-signature compatible, use
an admitted key, and match its private key. Configured DNS/IP identities are
exact SAN requirements. Windows enforces protected opened-handle/DACL rules;
Linux enforces `openat`/`O_NOFOLLOW`, ownership, size, regular-file, and mode
rules. Reload installs only a validated replacement and otherwise keeps the
last good still-valid key; expiry refuses new handshakes.

Deck system/private-CA modes remain chain and server-name based. Medium
security may capture only an otherwise-valid `UnknownIssuer` leaf, reject that
probe handshake, display actual hashes/validity and the peer address that
presented them, and accept an SPKI only after an explicit user action. The user
chooses between accepting for the current process and recording the identity in
`trusted_pins.json`; no Arcen protocol data precedes either. Every write to
`trusted_pins.json` (a remembered identity, its replacement after a confirmed
change, an imported connection's identity) changes one endpoint under an
advisory lock, through a synced temporary file renamed into place, and only
while that endpoint's identity on disk is still the one the user reviewed, so
one Deck window never rolls back another's. A recorded identity
is loaded before any connection and installs a pinning verifier rather than a
capturing one, so a subsequent mismatch is terminal and cannot be re-prompted.
Rekey requires new trust. Each concurrently dialled address captures into its
own slot, so a displayed fingerprint always belongs to the handshake that
produced it. The existing insecure Low mode remains separately double-gated, is
surfaced on screen while active, and is not a compatibility path for invalid
host material.

Secrets must not enter source, logs, workflow inputs visible to untrusted code,
or build artifacts. Security-sensitive changes require threat analysis, negative
tests, and Release/Security review.

## Microphone input boundary

Microphone input is off on the Deck by default and requires independent operator policy,
Deck consent, backend availability, authenticated-session binding, and additive
microphone-v1 negotiation. It uses a distinct sequenced upstream frame and never
reinterprets host-to-client audio. Frames before authorization, after disable,
for another generation, SID/WTS session, or Linux UID are rejected. Disable and
teardown stop capture/publication, drain bounded queues, and zero reusable audio
storage. Payloads and device-private data are never logged or persisted.
Deck consent is launch-only and never reloaded as enabled. A session-lifetime
cancellation latch stops callback publication and outranks queued data and
socket writes on every exit path.

Linux creates and restores its source only in the authenticated non-root session
user's audio environment. Windows keeps native handle/IOCTL ownership host-side
and requires a tightly ACLed SYSTEM feeder plus WTS, binary SID, and monotonic
generation enforcement for producers and capture readers. Windows performs no
default-recording-role mutation: the user or recording application selects the
installed endpoint, and disable/disconnect only stop and zero Arcen-owned audio.
Unreapable Windows driver I/O is fatal to the owning session-agent process, so a
stale worker cannot survive into a new attachment. macOS permission and capture
failures fail closed. See
[`../architecture/audio-input.md`](../architecture/audio-input.md) and
[`../operations/audio-input.md`](../operations/audio-input.md). A Windows Pier
offers no microphone until the complete, signed PortCls/WaveRT package is
installed; the checked-in ring contract alone is not a backend.

## Native video-codec boundary

The portable H.264 path accepts host-captured, checked I420 planes and emits
bounded Annex-B access units. `arcen-media` exposes a safe, pointer-free wrapper,
but `openh264-sys2` and the bundled Cisco C/C++ codec remain an unsafe native
memory boundary. Geometry, strides, output size, frame type, and recovery
parameter sets are validated around it; native initialization, encode, mixed
frame type, and oversized output failures close the media path rather than
switching codecs under an existing hello.

Only crates.io `openh264`/`openh264-sys2` 0.9.7 with the source feature are
admitted. No runtime loader, downloader, precompiled Cisco binary, codec
DLL/dylib/framework, or raw native pointer crosses into host policy. Exact
source and distribution controls are in
[`supply-chain.md`](supply-chain.md) and
[`../architecture/media-plan-resolution.md`](../architecture/media-plan-resolution.md).

## Deskside privacy boundary

Pinned deskside (`platform.deskside`, `platform.desktop.deskside`) is an
operator-enforced host policy, not a Deck request and not a wire capability.
It is off by default. Enabling requires fresh positive
physical-console evidence and complete operator pins; absence of known VM
markers is never positive evidence. Missing, unknown, virtual, remote,
paravirtual, conflicting, stale, unpinned, or capture-overlapping resources
refuse before mutation. Input and display controls are atomic, and protected
state is not reported until both verify.

Both hosts require CPUID to report no hypervisor and require an operator-pinned
hash of positively parsed firmware/chassis facts. Linux independently binds one
local `Remote=no` seat0 console session and its UID/DISPLAY/Xauthority while the
dedicated streaming PAM session remains `Remote=yes`. These checks classify
expected bare-metal evidence; they do not resist a hostile hypervisor.

Windows performs privileged display recovery through its existing
SYSTEM/Administrators-only versioned journal/watchdog and process-owned hooks in
the authenticated session agent. Arcen `SendInput` carries a process-random
marker, and physical or unmarked foreign injected events are swallowed.
Keyboard and mouse liveness are separately proven by bounded swallowed
canaries, and evidence-bound topology is continuously checked while streaming
and reconnect-held. The marker is not a
security boundary against a hostile same-desktop process able to observe and
replay hook metadata. Linux keeps evdev
descriptors, continuously revalidates the relevant input inventory, and physical
console ownership in the root launcher and persists only bounded normalized
console restore facts in a root-only `/run/arcen` journal. Both reject
symlink/reparse paths. Neither path logs or persists input events, keys, raw
EDID, serials, configured `/dev/input/by-id` values, raw monitor device paths, or
customer hardware identifiers; only bounded hashes, counts, stages, and stable
non-sensitive refusal classes cross diagnostic boundaries.

Linux treats every key/button or relative-axis device as relevant, refuses
absolute-axis classes, binds pins to canonical node inode/device identity, and
continuously revalidates evdev plus complete DRM/EDID/xrandr/DPMS state. Any
device/output add, removal, replacement, wake, or parse uncertainty drains.
All xrandr/xset/loginctl children are bounded, killed, and reaped on timeout.
Journal I/O failure cannot skip unblank; live guard cancellation schedules
display-first cleanup and retains the journal on incomplete recovery.

Resumable transport loss holds the same resources and authoritative deadline;
it never rearms or replaces ownership. Terminal drain stops remote injection and
media first, restores displays before releasing input, attempts every restore
after a failure, and preserves failed recovery state. This ordering limits both
privacy gaps and local denial of service. Pinned deskside makes no claim for
the secure desktop (Ctrl+Alt+Del), kernel HID, pens and tablets, physical
hot-plug, sleep and resume, or a driver reset.

Desk privacy is on by default on every Pier (`privacy.desk`,
[ADR 0025 §5](../adr/0025-arcen-joins-the-users-session.md)): the Deck's
user may turn it off for their own connections, an administrator may turn it
off or require it, every start logs the choice, and the Deck is told. Since
0.21.3 it is best effort unless required: a Pier that cannot prove no desk
monitor shows the session (a dummy plug on a GeForce, for example), or
cannot block the desk's keyboard and mouse, keeps every other measure,
logs a warning and tells the Deck that people at the desk may see or use
the session, also when a check fails during the session. Only with
`privacy.desk = "required"` does it refuse the attachment, or end it
(`console_privacy_unavailable`), as every Pier did before; sites whose
rules require a private desk must set it. The Windows
Pier detaches the desk's monitors from the desktop (SetDisplayConfig under
the display lease's recovery journal) and serves the session on a virtual
display; until the detach, a few seconds after a sign-in, the desk can see
the session (GAP-050). Desk pens and touch screens on Windows are not
blocked (GAP-051).

The macOS Pier's desk privacy (`privacy.desk`, on by default; rules in the
shared `arcen_session::console_session::desk_privacy`) runs in the signed-in user's
session helper, never at the login window and never in `serve`. It needs no
new privilege: the active `CGEventTap` needs the Accessibility grant keyboard
input already holds, and the transfer tables are the session's own. The
filter passes only events the window server attributes to the helper's own
process id (`kCGEventSourceUnixProcessID`); hardware, virtual HID and every
other process's synthetic events are dropped, so a process that can post
events as the helper can also pass the filter (the same non-claim as the
Windows marker). A display counts as dark when its table reads back dark;
the tables are re-asserted every 100 ms, so a display reconfiguration or
wake that resets them shows the desk the session for up to 100 ms. A display
that stays lit for two seconds, or a display list that cannot be read, ends
the Deck's session, locked first, when desk privacy is required; otherwise
the Deck is told and the session goes on. A console held by another account for
three consecutive readings (one a second) gets the desk back and ends the
Deck's session. A panic locks the session before the displays come back; a
crash does not: the tap's port dies with the process and the window server
restores a process's tables when it exits, so the desk gets the session
unlocked. Not covered: Secure Event Input (a focused password field) hides
keystrokes from every event tap, so the desk can type, unseen, into a
password field the Deck focused (the pointer stays blocked and the screens
dark); Siri, Voice Control, Switch Control, head-pointer input and
dictation act through accessibility and input methods, not HID events; a
display whose picture is itself a screen capture (DisplayLink, Sidecar,
AirPlay) may show the session although its table reads back dark
(untested). Nothing about the events is logged but counts. Between a sign-in typed at the streamed login window
and the Deck's reconnect to the signed-in user's helper (about two seconds)
the desk is not yet private: the login window's helper never arms it.

## Linux X11 virtual input boundary

A remote user's input reaches their dedicated Xorg desktop through kernel
uinput devices. No other input may reach it: not the physical or hypervisor
console's keyboard and mice, and not another session's Arcen devices.

Each dedicated Xorg ignores every input device unless it reports
vendor `a2ce` and its name carries the session token (`Arcen Virtual …
(session :N)`). The Pier's three devices match; ordinary console keyboards,
mice and tablets, and another session's Arcen devices, do not. The match is on
attributes a device chooses for itself, so it is a filter, not an
authentication: see the residual list. The Pier also never emits `KEY_SYSRQ`,
so a remote Alt+PrintScreen cannot reach the host kernel's SysRq handler.

Known limits:

- The admission match is impersonable. Vendor id and device name are
  self-declared: a crafted USB device that reports vendor `a2ce` and a product
  string containing `(session :N)`, or a local user who can open `/dev/uinput`
  (some distributions grant this through `uaccess` rules, for example
  `steam-devices`) and creates such a device, is admitted to that session's
  desktop. The filter stops ordinary console devices; it does not stop a
  deliberate impersonation. The planned fix: a shipped udev rule
  tags only virtual-bus devices (`id/bustype` `0006`, under
  `/devices/virtual/input`) with vendor `a2ce` through `ENV{ID_INPUT.tags}`,
  and the session's input class adds `MatchTag`, which closes the USB route.
  Restricting who may open `/dev/uinput` is host policy.
- The Arcen devices remain seat0 devices. A display manager greeter or a
  local session that becomes active on seat0 while an attachment exists (after
  a VT change, a display manager restart or a local logout) can open them and
  receive remote input. Every X11 sign-in activates its own session on seat0,
  which keeps the greeter inactive in normal operation. The fix is to assign
  the devices to a per-session seat with a shipped udev rule (the same rule
  that adds the virtual-bus tag above) and have the dedicated Xorg open them
  directly. It is not implemented yet.
- The kernel reads the Arcen keyboard like any other keyboard, whatever the
  seat. While the dedicated Xorg owns the screen the console keyboard is off,
  and the kernel acts only on SysRq (never sent) and on a secure-attention key
  if the console keymap binds one; if a local administrator switches the
  console to a text VT during a session, remote keystrokes reach that console.
- An experimental raw-HID device (absent from release builds) is not
  admitted by the dedicated Xorg on X11.
- A bridged Native tablet (`usb.native_tablet`, off unless the administrator
  enables it) reaches only its own desktop. Before its virtual port is connected, the helper installs a
  volatile udev rule for that port's subtree without `uaccess`, so no logind
  ACL reaches it. On a native-seat Wayland desktop the rule puts it on that
  desktop's seat. For a dedicated Xorg desktop it stays on `seat0` (a `seat0`
  Xorg hot-adds only `seat0` devices), its input nodes are tagged for Xorg
  (`arcen_bridge`, `arcen_bridge_<display>`), `LIBINPUT_IGNORE_DEVICE` is
  set and logind's `seat` tag is removed (logind does not track the nodes
  for a console session to take): the installed `/etc/X11/xorg.conf.d/90-arcen-bridged-usb.conf` makes
  every X server (GDM's greeter, console sessions, other desktops) ignore
  `arcen_bridge` before it opens the device. Each desktop's configuration,
  read after that directory, ends by ignoring every `arcen_bridge` again
  (its isolation classes admit Arcen's virtual devices by name and vendor
  id, which a bridged device's descriptors could imitate) and re-admitting
  only its own tag. Because such a device stays on `seat0`, where the
  kernel's console keyboard handler binds any device with keyboard keys, a
  tablet with a keyboard interface or keyboard keys is refused on X11. The Pier
  refuses to start a bridge unless every running X server is proven to keep
  it out (loaded that file, or is another desktop that ignores all foreign
  input) and its desktop re-admits it, and it verifies each node from the
  udev database. GDM's log does record that it saw and ignored the device.
  The isolation classes above are unchanged
  ([ADR 0021](../adr/0021-bridged-usb-devices-on-their-own-seat.md)).

The Wayland provider creates no kernel input devices and is outside this
boundary.

## Direct-QUIC resume boundary

Session resume (SessionAutoReconnect) applies to a Deck connected directly to
a Linux or Windows Pier over QUIC/TLS 1.3 on UDP 18444. An
opaque resume grant is a bearer secret: theft during its bounded lifetime can
attempt to take over the one detached session. TLS confidentiality is
necessary but not sufficient, so the grant additionally binds the validated
stable TLS SPKI host identity, active host session, native SID/WTS or
uid/logind principal, Deck holder nonce, disclaimer digest/version, and exact
client display/time-zone topology. It is not bound to a peer IP address.
Deck separately binds its memory-only controller to the configured endpoint,
TLS security mode/pin selection, topology, and connection generation. Every
attempt performs a fresh TLS handshake before sending the bearer grant; a TLS
pin or host-SPKI change is terminal rather than a compatibility fallback.

Each Pier process alone owns a fresh random 256-bit HMAC-SHA-256 key in
zeroizing memory. The key, grants, holder nonces, passwords, and credentials
must not be persisted, logged, formatted, or included in support bundles.
Typed debug output redacts secrets, received grant/nonce strings are cleared
after parsing, the shared `AuthResponse` wipes its password, holder nonce and
grant whenever it is dropped (so every early refusal on every Pier wipes them), temporary token bytes and signatures are zeroized, and native
principal values remain outside safe telemetry. Session Log IDs and
`previous_sid` are diagnostic chaining only.

One mutex-protected current generation+nonce slot gives exactly one concurrent
winner and rotates before handoff or attached refresh. Replays, malformed
signatures, stale generations, and duplicate attempts fail closed. The
opted-in host refreshes the one slot every `W/2` and sends the successor in a
successful in-band `AuthResult`; claims remain bounded to `2W` and at most
14,400 seconds. No old and new slot are simultaneously acceptable.

Refresh and resume success have no wire acknowledgment. If either successful
`AuthResult` is lost, the old token is already consumed and the successor is
unknown to Deck. A known post-rotation serialization/send failure revokes and
drains immediately. Otherwise, presenting the cryptographically valid,
unexpired exact predecessor presented to a detached slot is visibly rejected as replay and atomically
triggers the same final drain. After cleanup, normal credentials can create a
session; the old token is never reopened and the successor is never recovered
or disclosed. Older, ahead, malformed, wrongly signed, expired, or incorrectly
bound candidates cannot trigger owner drain. Invalid resume never falls back to
PAM, `LogonUserW`, Credential Provider, ordinary password auth, or a new
disclaimer decision.

Deck stores no deadline while connected. At unexpected transport loss both
Deck and host start an exact monotonic, generation-tagged `W` deadline; failed
resume retries preserve that same deadline. Exact equality is expired, and
stale/early timer callbacks are no-ops. Clock acquisition, arithmetic,
randomness, HMAC, lock, or generation-exhaustion failures issue no usable grant
and fail closed.
Restart loses the process key and slot, making all pre-restart grants invalid;
there is no database, cross-process recovery, cross-host state, or network key
fetch.

Topology and native checks are repeated after token rotation and before media
attachment. TLS SPKI rekey, display/topology/time-zone change, SID/WTS or
uid/logind change, native-session death, or agent/launcher failure is terminal.
The initial disclaimer digest/version remains fixed through resume. Windows
retains its display watchdog/journal and system-wide time-zone lease; Linux
retains the dedicated-Xorg display resources and process-tree time-zone lease.
Final drain revokes resume before restoring/releasing them.

**Same-user takeover (Linux Pier, ADR 0014 Amendment 6).** A session's
own user may continue it without its resume grant, in two cases only:

- *Detached session.* A fresh sign-in that passes full PAM authentication
  as the **same user** takes the detached session over at authentication
  (`arcen_session::direct_reconnect::fresh_sign_in` →
  `TakeOverDetached`): the owner retires the resume grant (its holder is
  recorded, so the old Deck's resume is refused with "this session was taken
  over by a newer sign-in of the same user"), releases the held display and
  session slot, and keeps the desktop, to which the newcomer reattaches.
  Another user is refused after authenticating and the session is
  untouched. With a sign-in disclaimer configured the detached
  authentication request carries none to acknowledge, so only resume
  continues such a session.
- *Attached session.* A sign-in never takes over a live attachment. The
  same user's newcomer may only prove the attached Deck's peer silent (no
  datagram for 3 s while the 1 s keep-alive runs); a silent or already
  timed-out attachment is then handed over the same way. Another user's
  attachment is never probed.

The Windows Pier applies the same two rules to its WTS session, keyed by the
account SID: a detached session is taken over through the resume registry,
and a held display is proved silent through the shared
`arcen_transport::quic::IncumbentAttachment`; a displaced attachment drains
its resume slot and tears down instead of entering the resume hold, and a
same-user sign-in that finds a live Deck is refused at once.

The trust consequence: knowledge of the user's password continues the
user's own detached desktop without the Deck that held it, exactly as it
can after the reconnect window expires; it never reaches another
user's desktop. Holding a stolen resume grant gains nothing once the owner
signed in again.

Adversarial unit tests cover tampering, every identity binding, the lifetime
bounds, rotation, replay, stale timers and redaction. They are not an
external audit, a certification, a production soak or a completed
adversarial assessment on each operating system.

See
[`../architecture/session-auto-reconnect.md`](../architecture/session-auto-reconnect.md)
and [ADR 0005](../adr/0005-direct-transport-resume-authority.md).

## Pre-session disclaimer

An enabled Pier sends exact, validated, bounded UTF-8 disclaimer text in the
protocol-v3 `AuthRequest`. Deck must display it before collecting credentials.
Only an explicit Accept may advance to credential entry and one `AuthResponse`
whose lowercase SHA-256 acknowledgment matches the exact displayed bytes.
Decline sends no `AuthResponse`; decline, close, timeout, absent acknowledgment,
invalid digest, and mismatch all fail before PAM, `LogonUserW`, Credential
Provider handoff, a privileged launcher, or session acquisition.

The acknowledgment is operational evidence, not authorization and not proof
that a human understood the text. It is not cryptographically bound into
`SessionGrantClaims` v1. After successful
OS authentication, host logs may record only locale, digest, host acceptance
time, correlation/session identity, and success. Banner text, credentials,
username/domain/SID/PAM account, peer data, and raw errors are excluded.
Support bundles inherit the existing log privacy boundary and never add the
banner source file.

Arcen makes no certification or legal-sufficiency claim for the wording an
operator configures.

Arcen makes no claim of certification, regulatory compliance, data residency
or completed cryptographic review.

## Support bundles

Support bundles are sensitive operational data, not anonymized reports. They
can contain redacted configuration, service state, lifecycle events, and raw
Arcen logs that may still identify usernames, domains, correlation IDs, or peer
addresses. Operators must protect bundles in storage and transit and review
them before sharing.

Collection is service-independent and uses per-platform source allowlists.
Credentials, TLS certificate/private-key files and configured paths,
Xauthority, customer payloads, proprietary payloads, arbitrary filesystem
trees, and hostnames in archive names/manifests are excluded. Collection must
not stat, open, hash, copy, or serialize either TLS material path or file. JSON
secret-bearing fields and native event host/security metadata are redacted,
while unavailable or denied sources remain visible as typed notices. Only
already-sanitized bounded TLS lifecycle metadata may enter through approved
logs/events.

The shared collector (`arcen_observability::support_bundle`, used by every
Pier and Deck) bounds source counts and sizes, streams files with fixed
buffers, rejects symlinks and (Windows) every reparse point, checks on Unix
that the file opened is the one inspected, caps included logs at 200 MiB and
total payload at 256 MiB on a Pier, and atomically publishes from a private
same-directory partial file. Custom
`--out` paths are operator-selected trust boundaries and retain their existing
directory permissions.

CI runner, dependency, artefact and release trust controls are in
[Supply-chain security](supply-chain.md). CI has no hardware or signing jobs.
