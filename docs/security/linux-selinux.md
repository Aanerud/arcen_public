# Linux Pier under SELinux

**Status:** Linux Pier 0.17.0. Needs Release/Security review (see
[Review items](#review-items)).

A Linux Pier on an SELinux-enforcing host (RHEL, Rocky, Alma and Fedora
default to it) serves desktops only with Arcen's SELinux policy module
loaded. The installer loads it on every host where SELinux is enabled and
unloads it on uninstall. The module adds one rule; nothing else about the
host's policy, booleans, labels or enforcement changes.

## How a session is labelled

The packaged Pier is a systemd service whose binary is labelled `usr_t`
(`/opt/arcen/bin/arcen-pier`), so systemd starts it, and every process it
spawns without a transition, as `system_u:system_r:unconfined_service_t:s0`.

| Process | Runs as | SELinux context |
| --- | --- | --- |
| Pier (`arcen-pier`) | root | `system_u:system_r:unconfined_service_t:s0` |
| Session launcher (`arcen-pier session-launcher`) | root | the Pier's |
| Launcher's own helpers: `loginctl`, `busctl`, Xorg, `xauth`, `xrandr`, `xdpyinfo` | root | the launcher's |
| Session agent and everything it starts (GNOME Shell or Mutter, the private D-Bus, PipeWire, WirePlumber, the AT-SPI bus launcher and registry) | the user | the context PAM assigned, by default `unconfined_u:unconfined_r:unconfined_t:s0` (D-Bus then enters `unconfined_dbusd_t` by the policy's own rules) |
| The compositor's Xwayland (X11 applications, `platform.session.wayland_x11_apps`) | the user | `unconfined_u:unconfined_r:xserver_t:s0`: the policy's own transition from `unconfined_t` on `/usr/bin/Xwayland` (`xserver_exec_t`); its `/tmp/.X11-unix/X<n>` sockets and `/tmp/.X<n>-lock` files are `user_tmp_t`. Arcen's module adds nothing for it |
| Release helpers (`fusermount`, `rm` of the private runtime directory) | the user | the context PAM assigned |
| Pier-spawned user helpers (capenc, audiocap) | the user | the Pier's (unchanged; see review items) |
| The user's `systemd --user` manager | the user | started by systemd itself: `unconfined_u:unconfined_r:unconfined_t:s0-s0:c0.c1023` |

## What PAM does, and where Arcen applies it

The launcher opens every desktop through the operator's PAM service
(`platform.auth.pam_service`, `login` by default). On SELinux hosts that
stack contains `pam_selinux.so close` … `pam_selinux.so open`. `open`
computes the user's default context from the login mapping
(`semanage login -l`; `__default__` is `unconfined_u`) and writes it as the
calling thread's *exec* context (`/proc/thread-self/attr/exec`), on the
assumption that the next `execve` is the user's session. PAM's own comment
in the stack says the same: "pam_selinux.so open should only be followed by
sessions to be executed in the user context".

The launcher's next `execve` is not the user's session. After PAM opens the
session it still runs root helpers (`loginctl` to prove the logind session,
`busctl`, Xorg, `xauth`). So, synchronously after `pam_open_session` returns
and before it spawns anything (`hosts/linux/src/session/selinux.rs`):

1. It reads the exec context PAM set and clears it from its thread, so its
   own helpers keep its domain.
2. It asks the kernel (`/sys/fs/selinux/access`, as `security_compute_av`
   does) whether its domain may make the `process:transition` into that
   context. If the policy says no, the open is refused at once with
   `SELinux policy does not let the launcher start the session in the
   context PAM assigned` (reason class `selinux_context_refused`) and the
   PAM session is closed. Nothing else is started.
3. It sets the context again only in the forked child that drops to the
   session user, immediately before that child's credential drop and
   `execve` — where a display manager's session worker applies it.

Entering the user's context is a domain transition, and the kernel clears
a process's parent-death signal on every such transition (as on a
credential change). So the session agent, first thing, arms its own
parent-death signal (`SIGTERM`) and checks that its parent is still the
launcher pid it was given (`--parent-pid`), exiting otherwise; its stop
handling, installed before any desktop process exists, turns that signal
into the same complete release a normal stop performs (desktop, stack,
session-bus services, private runtime directory), after which logind
closes the session. The release helpers (`rm`, `fusermount`) are single
bounded commands and simply finish if the launcher dies under them.

The context is never discarded. If the transition is refused at `execve` (a
policy the preflight could not query), the session is refused with the same
error; Arcen never runs a user's desktop in the service's domain instead,
because for a confined SELinux user that would remove their confinement.

Without SELinux nothing changes: where `/sys/fs/selinux/enforce` does not
exist (SELinux disabled, or another LSM such as AppArmor owns
`/proc/*/attr/exec`), or PAM set no exec context, there is nothing to take
or apply.

## Why a policy module is needed

The targeted policy (checked in `selinux-policy-targeted` 42.1.18 on
Rocky 10.2 and 38.1.75 on Rocky 9.8) lets display managers (`xdm_t`), `sshd_t` and
`init_t` transition into `unconfined_t`. It does not let
`unconfined_service_t`. With the launcher fixed as above, the session
user's first process is still refused:

```text
avc:  denied  { transition } for comm="arcen-pier" path="/usr/bin/loginctl"
  scontext=system_u:system_r:unconfined_service_t:s0
  tcontext=unconfined_u:unconfined_r:unconfined_t:s0 tclass=process permissive=0
```

(that record is from the unfixed launcher, whose `loginctl` carried the
user's exec context; for the fixed launcher the same rule is what its
preflight reports as denied for the session agent.) `unconfined_service_t` is in `process_uncond_exempt`, so the
user and role constraints do not apply; only that one TE rule is missing.

Arcen's module, `packaging/linux/selinux/arcen.cil`, adds exactly it:

```text
(allow unconfined_service_t unconfined_t (process (transition)))
```

The installer writes it to `/usr/share/arcen/selinux/arcen.cil` and runs
`semodule -i` on it (module `arcen`, priority 400). Uninstall runs
`semodule -r arcen` whether or not SELinux is still enabled, and removes the
file and directory. A unit test pins the module to that single rule, so any
widening shows up in review.

### What the rule grants

Nothing the source domain lacks. `unconfined_service_t` is already
unconfined (`unconfined_domain_type`, `can_load_policy`, `can_setenforce`,
…): it can already do anything `unconfined_t` can. The rule lets any process
in `unconfined_service_t` — not only the Pier — enter `unconfined_t` through
`setexeccon` plus `execve`, as unconfined logins already can. Confined
services are not affected. Fedora's and EPEL's `xrdp-selinux` package ships
the same rule for the same reason (xrdp's session manager is also an
`unconfined_service_t` service that runs `pam_selinux`).

### Alternatives considered

- **Clear the exec context and run the user's desktop in the service's
  domain** (no module). Rejected: it silently drops `pam_selinux`'s
  decision, so a user mapped to a confined SELinux user (`user_u`,
  `staff_u`) would run unconfined, and every user's files and processes
  would be labelled `system_u`.
- **An Arcen PAM service without `pam_selinux`, or with it moved.** Rejected
  for the same reason, and because placement cannot help: the transition
  itself is what the policy forbids. Arcen keeps using the operator's own
  PAM stack.
- **Labelling the Pier as a display manager (`xdm_exec_t`).** Rejected: it
  would confine the whole Pier (QUIC, NVFBC, uinput, capture supervision)
  in `xdm_t`, a large and fragile policy surface, to obtain one transition.
- **A dedicated `arcen_pier_t` domain.** Narrower in principle (only the
  Pier could make the transition) but needs a full domain definition
  written against policy internals that differ between EL9, EL10 and
  Fedora. A candidate for later; see review items.

## Limits

- **Confined SELinux users** (`user_u`, `staff_u`, `guest_u`, …) are not
  covered: the module adds no transition into `user_t`, `staff_t` or the
  like, so their sessions are refused with `selinux_context_refused`.
  Supporting them needs per-domain rules and testing.
- **MCS range.** `pam_selinux` bounds the user's level by the caller's, and
  the Pier runs at `s0`, so the session's processes get
  `…:unconfined_t:s0` rather than the full `s0-s0:c0.c1023` an SSH or GDM
  login gets. On a targeted host this matters only for objects carrying MCS
  categories (for example some container volumes).
- **Pier-spawned user helpers** (capenc, audiocap) run with the user's
  uid in the Pier's domain, as before; PAM's context reaches only processes
  the launcher starts.

## Operating it

Check the module and the labels:

```sh
semodule -l | grep -x arcen
ps -eZ | grep -E 'arcen-pier|gnome-shell'
```

A Pier on an SELinux host without the module refuses every session; the
Pier log and journal show the launcher's
`SELinux policy does not let the launcher's domain start the session
user's processes …` with `launcher_context` and `session_context`. Rerun
the installer, or load the shipped file by hand:

```sh
semodule -i /usr/share/arcen/selinux/arcen.cil
```

The development deployment (`packaging/linux/deploy-pier.sh`) loads the
same module when `/sys/fs/selinux/enforce` exists.

Reading denials: `ausearch` reads standard input instead of the audit log
when standard input is not a terminal (for example `ssh host ausearch …`),
and then reports `<no matches>`; add `--input-logs`:
`ausearch --input-logs -m AVC,USER_AVC,SELINUX_ERR -ts recent`.
`semodule -DB` disables `dontaudit` rules to show hidden denials;
`semodule -B` restores them.

Do not fix a refusal with `setenforce 0`, a permissive domain, or by
removing `pam_selinux` from the PAM stack.

## Evidence (Rocky 10.2, enforcing, GNOME 49)

Launcher started from a transient system service running the binary
directly (`unconfined_service_t`, exactly as the packaged unit), a local
test user, `dontaudit` disabled:

| Build | Result |
| --- | --- |
| Before the fix | `loginctl` `execve` → `EACCES`, AVC above; the open misreported as `pam_systemd did not create the required logind session` |
| Fix, no module | Refused before anything starts: `launcher_context=system_u:system_r:unconfined_service_t:s0 session_context=unconfined_u:unconfined_r:unconfined_t:s0`, `selinux_context_refused` |
| Fix and module | Headless Wayland harness `RESULT: PASS` (session, first frame, input, teardown); launcher `unconfined_service_t`, agent and desktop `unconfined_u:unconfined_r:unconfined_t:s0`; no AVC from the launcher or the session |
| Fix and module, with the Pier's Wayland server path merged | Harness `RESULT: PASS` again; `arcen-pier wayland qualify --user … --no-config` run as a transient `unconfined_service_t` service: session ready in 1.6 s, capture, input, rates and teardown `ok`, start-up selection `wayland_qualified`; no AVC from Arcen or the session |

The Xorg launcher shares the same code path. The Rocky 10 lab has no Xorg,
so its run reached the PAM session, the context take-over and the root
`xauth` `execve` (which failed `ENOENT`, not `EACCES`); the Rocky 9 Xorg
lab runs with SELinux disabled, where nothing changes.

### The accessibility bus in a headless Wayland session

A headless Wayland session has its own `dbus-daemon`, which runs as
`unconfined_dbusd_t`. When that bus activates `org.a11y.Bus`, it executes
`at-spi-bus-launcher` (`gnome_atspi_exec_t`), and the targeted policy
computes `unconfined_u:unconfined_r:gnome_atspi_t`, a context whose role is
not authorised for that type. The kernel refuses the `execve`:

```text
type=SELINUX_ERR op=security_compute_sid
  invalid_context="unconfined_u:unconfined_r:gnome_atspi_t:s0"
  scontext=unconfined_u:unconfined_r:unconfined_dbusd_t:s0
  tcontext=system_u:object_r:gnome_atspi_exec_t:s0 tclass=process
```

Every GTK application in the session then logs `AT-SPI: Error retrieving
accessibility bus address: … Failed to execute program org.a11y.Bus:
Permission denied`, and screen readers have nothing to talk to. The
accessibility bus's own `dbus-daemon` hits the same refusal when it
activates `at-spi2-registryd`. A login through GDM does not, because its
session bus is systemd's, which starts the launcher as a user unit
(`SystemdService=at-spi-dbus-bus.service`) from `unconfined_t`. Upstream
reports of the same policy gaps:
[fedora-selinux/selinux-policy#3385](https://github.com/fedora-selinux/selinux-policy/issues/3385)
and [#3386](https://github.com/fedora-selinux/selinux-policy/issues/3386).

The session agent therefore starts `at-spi-bus-launcher
--launch-immediately` and, once it owns `org.a11y.Bus`, `at-spi2-registryd`
itself, before the compositor. Both stay in the session's own
`unconfined_t`, exactly as under systemd, and nothing is activated through
the transition. No policy rule is added. On Rocky 10.2
(`selinux-policy-42.1.18-4.el10_2.4`, enforcing) a session started this way
logged no `gnome_atspi_t` error from its user, `GetAddress` returned the
private bus address, the registry answered `Peer.Ping`, and a GTK 4
application in the session started without the AT-SPI warning.

A launcher started through a `/bin/bash` wrapper runs as `initrc_t`, not
`unconfined_service_t`; lab runs must exec the binary directly to
reproduce the packaged domain.

## Review items

For Release/Security:

1. Ship the `arcen` CIL module with the single rule above (installer
   `semodule -i` / `semodule -r`, kept at
   `/usr/share/arcen/selinux/arcen.cil`).
2. Accept that the rule applies to every `unconfined_service_t` process, not
   only the Pier (precedent: `xrdp-selinux`), or commission a dedicated
   Pier domain.
3. Confined SELinux users are refused rather than supported.
4. Sessions run at `s0`, not the user's full MCS range; a
   `range_transition` (or running the Pier at `s0-s0:c0.c1023`) would
   change that.
5. Whether Pier-spawned user helpers (capenc, audiocap) should also run in
   the PAM-assigned context.
