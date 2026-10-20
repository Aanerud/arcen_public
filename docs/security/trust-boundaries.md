# Trust Boundaries

Arcen treats clients, hosts, gateway peers, identity-provider responses,
network metadata, and persisted configuration as distinct trust inputs.

## Required separations

- OIDC establishes user identity; Entra is the first provider profile.
- OS-specific machine authentication establishes host identity.
- Session authorization binds verified user, machine, and policy decisions to a
  short-lived session.
- Transport encryption protects a channel but does not replace authorization.

Commercial Pier licensing was withdrawn when Arcen moved to AGPL-3.0 free
software; see [ADR 0006](../adr/0006-offline-pier-licensing.md). The remaining
capacity-one session gate is a physical host-resource boundary, not an
entitlement boundary.

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
that the launcher activates, so up to and including the first 0.17.0 build
its remote user, and every session-less process of that user, held those
rights. Headless Wayland desktops are seatless and were not affected.

From 0.17.0 the installer installs, and the Pier rewrites at every start,
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

A seated desktop is refused before any session is opened while the rule is
not installed (`polkit_rule_missing`). After its session is active and
before it is handed over, every desktop is proved: the Pier runs `pkcheck`
against a process in the session for a fixed set of actions stock systems
grant local users. A seated desktop is refused, and its processes killed at
once, on any grant or missing answer (`polkit_grants_remote_session`,
`polkit_unverified`); a seatless desktop only on a grant. At start the Pier
also asks polkit about a local console session, when one exists, and warns
when the console is refused a grant its defaults promise.

Residual, recorded rather than claimed fixed:

- polkit older than 0.106 has no JavaScript rules. There an X11 desktop is
  still served, with a `SECURITY:` error at start and on every sign-in
  (`polkit_no_rules`); refusing it instead is an owner decision.
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
what one would. Each step is reviewed as a boundary (Release/Security):

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
  stays locked and applications ask, as before. A second desktop of a user
  who is also signed in at the console runs a second keyring daemon on the
  same keyring files, as activation already did before this; gnome-keyring
  re-reads keyring files that change.
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
  Clutter, Cogl and GNOME Shell switches (such as `MUTTER_DEBUG_FORCE_HDR`),
  EGL/GLX/Mesa/GBM driver selection and `GSETTINGS_BACKEND` (which would
  bypass the pinned settings) are denied. The compositor's command removes
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
`trusted_pins.json`; no Arcen protocol data precedes either. A recorded identity
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

Microphone input is disabled by default and requires independent operator policy,
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
[`../operations/audio-input.md`](../operations/audio-input.md). Windows remains
unavailable until the complete PortCls/WaveRT package is installed; the
checked-in ring contract alone is not a backend.

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

Deskside is an operator-enforced host policy, not a Deck request and not a wire
capability. It is disabled by default. Enabling requires fresh positive
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
privacy gaps and local denial of service. SAS/secure desktop, kernel HID,
pen/tablet, physical hot-plug, sleep/resume, and driver-reset behavior are
explicit non-claims pending physical lab evidence.

## Linux X11 virtual input boundary

A remote user's input reaches their dedicated Xorg desktop through kernel
uinput devices. The goal is that no other input reaches it. Up to and including
the first 0.17.0 build, the dedicated Xorg hot-added every input device on
seat0: the physical or hypervisor console's keyboard and mice could type and
click in a remote desktop the console cannot see, and a second dedicated Xorg
on the same host received another session's Arcen devices.

From 0.17.0 each dedicated Xorg ignores every input device unless it reports
vendor `a2ce` and its name carries the session token (`Arcen Virtual …
(session :N)`). The Pier's three devices match; ordinary console keyboards,
mice and tablets, and another session's Arcen devices, do not. The match is on
attributes a device chooses for itself, so it is a filter, not an
authentication: see the residual list. The Pier also never emits `KEY_SYSRQ`,
so a remote Alt+PrintScreen cannot reach the host kernel's SysRq handler.

Residual, recorded rather than claimed fixed:

- The admission match is impersonable. Vendor id and device name are
  self-declared: a crafted USB device that reports vendor `a2ce` and a product
  string containing `(session :N)`, or a local user who can open `/dev/uinput`
  (some distributions grant this through `uaccess` rules, for example
  `steam-devices`) and creates such a device, is admitted to that session's
  desktop. A1 stops ordinary console devices; it does not stop a deliberate
  impersonation. The fix is part of the follow-up below: a shipped udev rule
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
  directly; it needs installer and Release/Security review.
- The kernel reads the Arcen keyboard like any other keyboard, whatever the
  seat. While the dedicated Xorg owns the screen the console keyboard is off,
  and the kernel acts only on SysRq (never sent) and on a secure-attention key
  if the console keymap binds one; if a local administrator switches the
  console to a text VT during a session, remote keystrokes reach that console.
- A bridged Hard USB tablet and an experimental raw-HID device (both absent
  from release builds) are not admitted by the dedicated Xorg on X11.

The Wayland provider creates no kernel input devices and is outside this
boundary.

## Direct-QUIC resume boundary

The draft SessionAutoReconnect implementation applies only to macOS Deck
directly connected to a Windows or Linux Pier over QUIC/TLS 1.3 on UDP 18444. An
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
after parsing, temporary token bytes and signatures are zeroized, and native
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

The trust consequence: knowledge of the user's password now continues the
user's own detached desktop without the Deck that held it, exactly as it
already could after the reconnect window expired; it never reaches another
user's desktop. Holding a stolen resume grant gains nothing once the owner
signed in again.

Adversarial unit tests cover token/signature tamper, every identity binding,
exact `2W` lifetime bounds, attached rotation, malformed/oversized tokens, one
concurrent rotation winner, exact-predecessor terminal replay, older-token
rejection, unauthenticated topology mismatch without attacker-triggered drain,
successor-send failures, stale timers, redaction, and one-time crash/shutdown
cleanup. Protocol tests cover old-peer JSON compatibility, strict in-band
refresh shapes, and stable failure strings; Deck tests restrict retries to
selected transient I/O and require a rotated successor before media. These
tests do not establish an
external audit, certification, production soak, real clock-manipulation proof,
or completed target-OS adversarial assessment.

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
`SessionGrantClaims` v1; a signed-schema v2 rollout is deferred. After successful
OS authentication, host logs may record only locale, digest, host acceptance
time, correlation/session identity, and success. Banner text, credentials,
username/domain/SID/PAM account, peer data, and raw errors are excluded.
Support bundles inherit the existing log privacy boundary and never add the
banner source file.

This behavior and operator-provided legal text require Shared/Architecture,
affected component-owner, and Release/Security/legal review. Arcen makes no
certification or legal-sufficiency claim for the configured wording.

The initial scaffold makes no claim of certification, regulatory compliance,
data residency, or completed cryptographic review.

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

Host adapters bound source counts and sizes, stream files with fixed buffers,
reject symlinks/reparse points, cap included logs at 200 MiB and total payload
at 256 MiB, and atomically publish from a same-directory partial file. Custom
`--out` paths are operator-selected trust boundaries and retain their existing
directory permissions.

CI runner, dependency, artifact, and release trust controls are detailed in
[Supply-chain security](supply-chain.md). Protected hardware and signing jobs
must remain disabled while their environment-review controls are unavailable.
