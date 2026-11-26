# ADR 0025: Arcen joins the user's session

**Status:** Accepted 2026-11-16 (proposed and proved the same day); amended 2026-11-17 (§10: a real login session on hosts without a display manager; §3 domain accounts; §4 stop safety; §8: outputs from the live X server, guest mode on another product's desktop; §8 again in the 0.20.2 re-issue: a guest desktop is mapped onto the Deck by arrangement); amended 2026-11-18 (§5: desk privacy on by default on every Pier, `privacy.desk`); amended 2026-11-24 (§5: desk privacy is the Deck user's choice, best effort unless the administrator sets `privacy.desk = "required"`); amended 2026-11-25 (§8: another product's desktop takes the Deck's monitors on attach while that product has no client attached, and keeps them when the Deck leaves; §4 and §12: under `privacy.desk = "required"` a desk whose session will not lock stays private until it locks; §4 and §12 again: the Windows Pier locks a session held for its Deck's resume at once, and locks first under `required`). Product-wide; the Linux Pier is its main
part. Builds on [ADR 0005](0005-direct-transport-resume-authority.md)
(resume), [ADR 0008](0008-virtual-display-for-windows-hosts.md) and
[ADR 0014](0014-quic-lanes-and-capture-contracts.md); changes the Linux
session model of ADR 0010's dedicated output provider.

> **"Arcen is an extension of your machine — not a locked-in new desktop
> every time."**

That is the product principle this ADR records, for every Pier:

1. **Every Pier joins the session the user already holds.** A Pier never
   builds a private desktop for a user who has one.
2. **One session per user, entered from either side** — the desk, Arcen, or
   another remote-access product.
3. **A remote-first sign-in creates the operating system's real session**,
   so a later sign-in at the desk resumes it.
4. **Disconnect locks the session; it never ends it.** The session lives
   until the user logs out.

## Context

### What the Linux Pier did

Until this ADR every Linux sign-in opened a new PAM session through the
Pier's own PAM service, started a dedicated Xorg on virtual terminal 1 and
ran only `gnome-shell --x11 --sm-disable --replace` in it. A field report
(GDM X11 workstation, NVIDIA, another remote-desktop product installed) showed what that model
costs:

- The desktop was not the user's. No session manager, settings daemons or
  autostart: no desktop icons, logout and switch-user did nothing, and a
  studio application would not launch. Applications the user had open at
  the desk were not there.
- The desktop died with the reconnect window. An explicit disconnect, or a
  lost connection that did not come back within
  `auth.reconnect_window_secs`, ended it with everything in it.
- A sign-in at the desk later started a second desktop: GDM resumes only
  sessions it started itself (below).
- Another product's X server on the same GPU (its own Xorg on VT 7) made the dedicated Xorg hang after loading the NVIDIA driver; the
  Deck saw a timeout.
- At the `critical` log profile none of those failures left a line, and the
  failed session's directory (with its Xorg log) was removed.

The Windows Pier (the WTS console session, `hosts/windows/src/windows_session.rs`)
and the macOS Pier (the logged-in Aqua session, `hosts/macos/src/console.rs`)
already attached to the user's session. Linux was the exception.

### How GDM and GNOME behave (GDM 40.1, GNOME Shell 40.10; RHEL 9 family)

Read from upstream source and confirmed on a Rocky 9.7 lab workstation
(GRID vGPU, GDM X11 with `WaylandEnable=false`):

- **GDM resumes only sessions it started.** After a greeter sign-in GDM
  looks for an existing session of that user on the seat in its own list
  of sessions it launched, activates it and unlocks it; only if there is
  none does it start one (upstream `daemon/gdm-manager.c`,
  `switch_to_compatible_user_session` and `find_session_for_user_on_seat`).
  A session started outside GDM is invisible to it, so a desk sign-in
  creates a second desktop.
- **A GDM X11 user session** is a logind session `Class=user Type=x11` on
  `seat0` with its own VT (VT 7 on the lab host; the greeter is on VT 1),
  its own root Xorg (`-auth /run/user/<uid>/gdm/Xauthority`), logind
  `Display=` empty. The session scope holds the session worker,
  `gdm-x-session`, Xorg and `gnome-session-binary`; `gnome-shell` runs in
  the user's systemd manager, not the scope.
- **The lock screen** asks GDM for a reauthentication channel; a logind
  `Unlock` also dismisses it. `LockedHint` reports it.
- **The login screen has a pre-authenticated credential hook.** GNOME
  Shell's greeter (`js/gdm/oVirt.js`, `credentialManager.js`, `util.js`,
  present in GNOME Shell 40 and 49) watches the system-bus name
  `org.ovirt.vdsm.Credentials` for a `UserAuthenticated(token)` signal.
  When it arrives, the greeter starts its `gdm-ovirtcred` PAM conversation
  without a user name and answers that conversation's secret prompt with
  the token; whatever the PAM stack decides becomes the login, and GDM
  starts (or resumes) the session itself.
- **Automatic login** happens once per boot and needs a GDM restart to
  trigger again; timed login likewise reads its configuration at start.

## Decision

### 1. One model for every Pier

| Pier | Joins | Remote-first sign-in | On disconnect |
| --- | --- | --- | --- |
| Linux (this ADR) | The user's GDM X11 session, or the user's own session in another remote-desktop product | Through GDM's login screen (§3) | Lock; monitors and input given back |
| Windows | The WTS console session | Through the Credential Provider; Winlogon creates the console session | See the audit (§12) |
| macOS | The logged-in Aqua session | The Deck types into the streamed login window | See the audit (§12) |

Every OS-free decision of the Linux model lives in
`arcen_session::console_session` (`shared/session/src/console_session/`),
so the other Piers can adopt the same vocabulary: the console decision,
the refusal codes, the desk-input rule, the hand-back order and the
one-time token format. The Linux Pier (`hosts/linux/src/session/console.rs`,
`launcher.rs` `run_console`) only gathers facts and acts on the answers.

### 2. Linux: which session a sign-in joins

`platform.session.mode` is `console` by default. The launcher still
authenticates the password with the Pier's PAM service and runs the account
checks (`pam_authenticate`, `pam_acct_mgmt`), but opens **no PAM session**:
the session already exists, or GDM creates it.

It reads every logind session (`loginctl list-sessions`, then
`loginctl show-session` for each) and `console_session::decide` answers.
The read must be complete (`console_session::logind_read`, amended
2026-11-20): a list that fails, times out or is not a session list, a
listed session logind will not describe while it still lists it, or a
description that names another session or user than the list, is
"unknown", never "nobody", and refuses `console_sessions_unreadable`.
Only a complete read proves that the account has no session.

| Console | Decision |
| --- | --- |
| The account's own X11 session is active on `seat0` | Join it |
| The account's own X11 session waits behind the login screen (switch user) | Bring it forward (logind `ActivateSessionOnSeat`), then join it |
| Another account's session is active on `seat0` | Refuse `console_other_user` |
| Another account's session the display manager did not start (another remote-desktop product) exists on `seat0`, active or not | Refuse `console_other_user_other_product` |
| Only the login screen (an X11 greeter on `seat0`, active or idle) | Remote-first sign-in through it (§3) |
| The account's own session is Wayland | Identify it and prove its compositor read-only (`console_session::wayland_join`), then join it (§9, S4b); a Pier built without the Wayland adapter refuses `console_wayland_session` |
| The login screen's next session is Wayland | Refuse `console_wayland_session` (§9) |
| Nothing graphical on `seat0`, and the host has no display manager | Start the account's real login session (§10), then join it |
| Nothing graphical on `seat0`, and the display manager's login screen is not running | Refuse `console_no_session` |
| logind's sessions could not be read completely | Refuse `console_sessions_unreadable`: nothing is joined and no session is started |

An Arcen dedicated desktop (logind `TTY=arcen`) is never a console. Any
other session on `seat0` is, whatever its PAM remote host says. A session
whose PAM service is not GDM's (`gdm-*`) was started by another product
(`ConsoleOrigin::OtherProduct`). The refusal names that product's service
as the host reports it at run time (`console_session::ForeignService`): the
session's PAM service (logind `Service=`) and the system service that
started it (the `<name>.service` cgroup of the session leader or its nearest
ancestor in one; when logind has moved the leader of a `PAMName=` service
into the session scope and its parent is PID 1, the unit whose
`/run/systemd/units/invocation:<unit>` matches the leader's
`INVOCATION_ID`) with that process's name. Each part is kept only when it
is a short identifier (`[A-Za-z0-9._@:+-]`, at most 40 bytes), re-checked
where the broker reads it, and the Deck's text fits the 120-byte close
reason (`ConsoleNotice::deck_reason`). Arcen carries no product names of
its own.

The session's display is read from the session's own processes (the scope
`session-<id>.scope`, owned by the user): `DISPLAY` must be local (`:N`), the
socket `/tmp/.X11-unix/X<N>` must exist, and `XAUTHORITY` (products outside the
display manager write `~/.Xauthority`) must be a regular file owned by the user or root and
writable by nobody else (`console_session::xauthority_trustworthy`). The
launcher opens the display as root with that cookie
(`console_session::mit_magic_cookie`) only for privacy; capture and input
run as the user, exactly as before.

### 3. Remote-first sign-in through GDM

With nobody signed in, the sign-in must be completed **by GDM**, or the desk
cannot resume the session later. The launcher uses the greeter's
pre-authenticated credential hook:

1. The desk's input is disabled first (§5), so nobody at the desk can use
   the session the login screen is about to start.
2. 32 random bytes become the token. Its record — user name, uid, a
   deadline 30 seconds out — is written root-only (`0600`) to
   `/run/gdm/arcen-greeter-login/<SHA-256 of the token>` (directory `0700`
   root, inside GDM's own runtime directory so the session worker's SELinux
   domain, `xdm_t`, reads it without a policy change). The token itself is
   never on disk.
3. The launcher owns `org.ovirt.vdsm.Credentials` on the system bus (a
   policy file lets only root own it) and sends `UserAuthenticated(token)`
   **unicast to the login screen's own bus connections** (the
   `gnome-shell` processes of the greeter's own account in the greeter
   session's exact scope, `/user.slice/user-<uid>.slice/session-<id>.scope`,
   which a user cannot imitate), never as a broadcast.
4. The greeter starts `gdm-ovirtcred`. Arcen's stack for it starts with
   `pam_arcen_greeter.so` (`hosts/linux/pam-greeter`), which asks for the
   token, finds its record by hash, **removes the record before believing
   it** (a presented token is spent, admitted or not), checks the record
   with `greeter_token::admit` (deadline, uid not 0, the name still
   resolves to the same uid, the same user if the greeter named one) and
   sets `PAM_USER`. The stack's `account` and `session` lines are the
   host's own `gdm-password` ones, so the session is set up exactly like a
   password sign-in (account policy, `pam_systemd`, SELinux, namespaces);
   `password` is `pam_deny`.
5. GDM starts the session (`Service=gdm-ovirtcred`), or jumps to the user's
   existing GDM session. The launcher waits for it (30 s bound,
   `console_sign_in_failed` otherwise), joins it and makes it private.
6. With the desk private, the launcher waits until GDM has ended the login
   screen that took the sign-in (a few seconds after the session starts; at
   most 10 s and within the launcher's 55 s budget, shared
   `console_session::settle_sign_in`). GDM 40 puts a new login screen in
   front when that one ends while it still counts the login VT as active
   (on the lab host it did so on every sign-in for an hour, alongside
   greeter crashes in the GPU driver). The launcher then brings the user's
   session forward again (logind `ActivateSessionOnSeat`, at most three
   times in each of the two waits), exactly what the person who signed in
   would do, and keeps the monitors blank meanwhile. Another account's
   session in front is never switched away. A session that is not in front
   when the bound runs out is secured and the sign-in is refused
   (`console_session_unreachable`).

*Amended 2026-11-18 (GDM's login screen did not come back after a
session ended).* When a console session ends, GDM 40 opens a new login
screen session on terminal 1 and, before it starts the X server, switches
the console to that terminal and waits for the switch (`VT_ACTIVATE`,
`VT_WAITACTIVE`). The switch is asked of the ending session's terminal,
which logind still holds in process mode. If the login screen opens
quickly — the display manager's account still has a running user manager,
kept alive by a login screen that is still closing (an `ibus-daemon` a
crashed greeter shell left behind, or the login screen that just took a
sign-in) — logind restores that terminal to automatic mode before it
answers the request, and restoring the mode drops the pending switch. GDM
then waits forever: the login screen session is `online`, never `active`,
with no X server or shell; the desk shows an empty text terminal; and every
remote-first sign-in was refused `console_sign_in_unavailable`. Lab (GDM
40.1, systemd 252): reproduced deterministically with a process left in a
closing login screen's scope, and by ending a session a few seconds after
its sign-in; GDM's worker sits in `__vt_event_wait`, and switching the
console to terminal 1 releases it. The same defect occurs with 0.20.1.
Shared `console_session::login_screen` decides when the console is in that
state (a login screen on `seat0` that is `online`, not in front, without
any process of its own account, while no session is in front of the
seat). Before a remote-first sign-in the launcher brings such a login
screen to its terminal (logind `ActivateSession`, at most twice) and waits
(15 s bound) for its shell; between sign-ins the Pier watches the console
(every 2 s, after a 5 s grace, at most three times per login screen, only
while no console launcher runs) so the person at the desk gets the login
screen back too. Arcen never switches a session in front away. A shell
that started a moment ago may not listen for the token yet: while nothing
has spent it, the token is given again after 5 s (at most three times; the
record is checked again just before each signal, so a token whose
conversation already started is never sent again: the shell would restart
it). A login screen that is `closing` counts
as finished when the sign-in settles, and once it has, the finished login
screen's leftover processes are ended (logind `TerminateSession`, only for
a `closing` greeter session whose worker is gone), so they do not keep the
next login screen's race open. `ibus` itself is not touched.

**Security analysis.** The Pier issues a token only after the user's own
password passed the Pier's PAM service and account checks; the token
replaces that password inside GDM, nothing more.

| Threat | Mitigation |
| --- | --- |
| Replay | The record is removed on first presentation; a second use finds nothing. |
| Late use | Deadline 30 s; a deadline further out than one lifetime is refused (clock moved). |
| Eavesdropping on the bus | The signal is unicast to the greeter's connections; the token is never broadcast. |
| A process claiming the bus name | The system bus policy allows only root to own it; the greeter follows the name's owner. |
| Forged records | Root-only `0700` directory, `0600` single-link regular file owned by root, opened by descriptor without following links; anything else is refused (and spent). |
| Another PAM client using the stack | A process that is not root cannot read the records, so a token admits nobody outside GDM's root session worker. |
| Root or a vanished account | uid 0 is refused; a name that no longer resolves to the recorded uid is refused. |
| Account policy bypass | `pam_acct_mgmt` ran in the Pier, and the stack's own `account` lines run again in GDM. |
| A desk user grabbing the new session | Desk input is grabbed before the token is sent; the new X server's monitors are blanked as soon as it answers (a few seconds of a fresh desktop may show on a monitor; no input reaches it). |
| Overwriting another product | The installer writes `/etc/pam.d/gdm-ovirtcred` and the bus policy only when absent or Arcen's (marker line); otherwise it keeps them and the sign-in is refused `console_sign_in_unavailable`. |
| Audit | The PAM module logs every acceptance and refusal (`pam_arcen_greeter: … accepted for <user>` / `token refused (<reason>)`) to the journal; the Pier logs the sign-in and `SESSION_AUTH_OK`. |

**The login keyring** (amended 2026-11-18). No password reaches GDM with a
token, so `pam_gnome_keyring` starts the user's keyring daemon locked. The
launcher keeps a copy of the password the Pier just proved (zeroized, until
the session's agent is up) and, for a sign-in that started the session
(through the login screen, or Arcen's own login session, §10), hands it on
standard input to a short-lived helper running as the session user
(`session-agent --login-keyring-unlock`): it waits (30 s bound) for the
user's keyring daemon, checks the daemon's peer credentials and sends the
one `UNLOCK` request `pam_gnome_keyring` sends on the daemon's control
socket (shared `desktop_session::keyring_control`), which opens the login
keyring or creates it with that password. A joined existing session is not
touched; every failure is logged and the desktop asks for the keyring
password as before.

**Domain accounts.** On a host whose SSSD enforces Active Directory Group
Policy access control, SSSD decides a PAM service's logon right by its name
and denies an unknown one (`ad_gpo_default_right = deny`), so a domain
account's remote-first sign-in was refused in the stack's `account` phase.
The sign-in is an interactive logon — the user is placed in front of the
workstation's own desktop — so the installer adds the service to that
domain's `ad_gpo_map_interactive` (a `conf.d` snippet, the domain's own list
kept, an administrator's explicit removal respected; shared decision
`console_session::domain_logon`). It is never given a broader right
(`permit`, remote interactive); a service the administrator mapped to
another right is left to that map (SSSD refuses a service in two maps). The
snippet holds a copy of each domain's list (SSSD keeps one value per
option), so the Pier warns at start when it is stale. A sign-in the account
check refuses with "permission denied" is logged as `console_account_denied`
but shown on the Deck like any failed sign-in, so a refusal never confirms
a password (security review); the Deck sees the code when only the
remote-first sign-in is refused after the Pier's own check passed. The
Pier refuses a domain account at once (`console_sign_in_unavailable`) when
SSSD would not weigh the login screen's service as interactive.

**Alternatives considered.**

- *Typing the password into the greeter* (XTest): exposes the password to
  the greeter's X server and any client holding its cookie, depends on the
  greeter's layout and keyboard map. Rejected.
- *One-shot automatic login* (`AutomaticLogin` in GDM's runtime
  configuration, then a GDM restart): password-less while the flag is set,
  a restart that ends every other GDM session, and a stale flag logs the
  user in again at the next GDM start. Rejected.
- *An Arcen PAM session beside GDM* (the old model, or a logind session
  activated by Arcen): GDM never resumes it (F1). Rejected; this is the
  defect.

### 4. Lifetime: disconnect locks, never ends

| Event | What happens |
| --- | --- |
| Explicit disconnect, a Deck that did not ask for resume, or the reconnect window ends | The console is handed back (below) and the launcher exits; the session continues |
| Transport lost (held for its resume) | The console is handed back at once, with its sound; a resume makes it private again and unlocks it (below) |
| While held: another product's client takes the desktop, that product starts a desktop of its own, or someone unlocks or signs in at the desk | The hold ends at once and the launcher exits (`console_session::detached_hold`) |
| A resume finds the console changed | The console is decided afresh just before anything is touched (`console_session::decide_reattach`, `console::reattach`): only the held session of the same account, still the one the decision attaches, is joined; anything else refuses without touching the desk. The launcher answers the broker's `attach` with `attached` or the typed refusal, and the broker waits for that answer (25 s bound) before it streams: the returning Deck is shown the code (`console_session_changed` when the account's session is no longer the one it joined), never a capture error |
| Same user signs in again | Joins the same session |
| The user logs out (at the desk is impossible while attached; from the Deck) | The attachment ends |
| logind does not answer about the joined session (`loginctl` fails or is slow) | Not a logout: the launcher keeps the attachment and looks again. It ends only when logind no longer has the session (its state file is gone too) or describes it as no longer the account's console desktop. After `UNANSWERED_LOOKS_BEFORE_HAND_BACK` (5) unanswered looks in a row, about 10 to 25 s, ownership is unproven: the attachment ends and the console is handed back as when the Deck leaves (locked, or kept private while it cannot be locked); the session is never ended (`console_session::session_watch`, amended 2026-11-20) |
| The Pier or its launcher dies | The kernel releases the input grabs; the monitors stay blank; the Pier locks the session, then restores the monitors, from its journal (`/run/arcen/console-privacy.json`, directory kept by the tmpfiles rule) when the launcher is cleaned up and at service start |
| A refusal after the login screen started, or logind brought forward, the session | The session is handed back locked before the refusal |
| The session will not lock (no lock screen, lock disabled) | GDM is asked for a login screen on a new VT (`CreateTransientDisplay`) so the session goes to the background; failing that, the desk stays blank and its input disabled while the launcher retries. From the first failed hand-back the session's X server is also told to ignore the desk's devices (XInput `Device Enabled` 0, journalled): an `EVIOCGRAB` ends with the launcher, the X server's state does not. A launcher that is stopped or killed therefore leaves the desk blank and unusable; the Pier retries every 10 s (`console::guard_unsecured_console`, also re-blanking a monitor that came back and disabling a newly plugged device) and gives the desk its monitors and devices back only once the session is locked, behind a login screen, or gone. One Pier-wide gate keeps a retry and a console launcher apart. *Amended 2026-11-18:* a display manager's session is "behind a login screen" only for the desk's picture: the login screen is put in front, but the session stays private (blank, the desk's devices disabled in its X server) until it locks, because a key chord switches to its terminal (`console_session::unlocked_hand_back` returns `KeepPrivate` for it in the background), for at most a minute (`UNLOCKED_GRACE`, counted from the first failed lock in `/run/arcen/console-unlocked.json`): a session whose lock screen is turned off by policy never locks, and its owner signing in at the login screen must get it back. Another product's session in the background is given back as before |

**Resume order** (`console::reattach`): a session that waits behind a
login screen (a hand-back whose lock failed puts it there unlocked) is
brought forward only after the desk's input is disabled and its monitors
are blanked (Xorg keeps an inactive VT's gamma ramps and applies them when
the VT comes back; they are proved blank again once it is in front); it is
unlocked last. A first sign-in that brings a background session forward
follows the same order. A console that will neither lock nor go behind a login
screen keeps the desk private after the Deck's stream ends, retried every
5 s, until it does or the session ends. Any failure on
the way hands the console back as below, and a launcher that exits still
holding the desk's input hands it back too.

**Hand-back order** (`console_session::DETACH_STEPS`): lock the session,
wait (bounded) for the lock screen, give the monitors their own gamma ramps
back, give the desk its sound back (the session agent ends its audio lease),
release the desk's input. The monitors never show an unlocked desktop
and nobody can act before the screen is locked (`console::hand_back` stops
at the first step that cannot be proved).

Joining a locked session unlocks it through logind after the Pier's PAM
proved the owner's password and account, as a reattachment always did. The
launcher unlocks it, and only after the desk is private again; the broker
never unlocks a joined console.
Polkit's remote-session rule (`05-arcen-remote-session.rules`) does not
apply: the joined session is the user's own local session, with the rights
the user has at the desk.

*Amended in the 0.20.2 re-issue (field: a desktop another product owned):*
a joined console used to stay joined after its Deck left — a Deck that did
not ask for resume kept the launcher and session agent with no end (the rule
for Arcen's own dedicated desktops), and the agent's audio lease kept the
user's default output on Arcen's silent sink with the speakers muted until
the user logged out. What a held console keeps is now only what a resume
needs (an idle launcher and agent; no audio lease, devices or layout of a
guest console), for at most `auth.reconnect_window_secs`, and the hold is
watched every 2 s (`detached_hold::DetachedHold`): the other product's
client (a guest console), the other product's own new desktop, a session
unlocked again after the hand-back locked it, or another desktop in front
and unlocked, end it at once. *Amended 2026-11-25 (§8):* a guest console
laid out to the Deck keeps the Deck's layout through the hold and after it,
as every departure from it does.

*Amended 2026-11-25 (`required` fails closed when the lock fails):* a
private desk whose session refused to lock was given back unlocked after
`UNLOCKED_GRACE` on the macOS Pier whatever `privacy.desk` said, so
`required` did not fail closed at the moment it matters most. The decision
is shared (`console_session::lock_wait`, `WaitingDesk::Required`): under
`required`, a desk the Pier made private is never given back unlocked.
Past the grace it stays dark with its input blocked, the Pier's session
slot stays held (`SessionAdmissionRuntime::set_lock_refused`, so a Deck
that signs in meanwhile is told why the host is busy rather than only that
it is), the hold is logged once at error level (the asks that follow are not errors of their own), and the lock is asked
for again every `HELD_LOCK_RETRY` (30 s), with no end, until the session
locks or the console leaves the account; the desk watch reads the console
fail-closed as the lock does (`console::console_is_ours`), so a failed
read is not a departure of the account. `"on"` keeps the grace (the
Deck's user chose privacy; the owner at the desk must get the session
back), and `"off"` has no private desk to hold. Recovery is the
administrator's: turn the lock screen back on (the next ask locks), or
sign the session out. A Pier process that dies during the hold is the lock
guard's (§12): it locks, and gives the desk back after the grace without
the input filter, which is the agent's. The macOS Pier adopts it, and so
does the Windows Pier (below). The Linux console's own wait already keeps
a desk private until
it locks, for every mode, except a display manager's session it put behind
a login screen, which is given back there after the grace; whether
`required` holds that one too is open.

*Amended 2026-11-25 (Windows: a held session locks at once; `required`
locks first):* the Windows Pier left a session held for its Deck's resume
unlocked for `auth.reconnect_window_secs` (GAP-044), and gave the desk its
outputs and input back before it locked (PD-033), so under `required` a
refused lock left the desk live. Both are decided in shared code now
(`console_session::held_lock`, `console_session::hand_back_order`).

When a Deck's transport is lost, the hold runs `HOLD_STEPS`: the session
agent stops streaming and stays, keeping the desk as private as the
attachment had it; the session is locked (`LockWorkStation`, or taken off
the console if it will not lock), as `desktop_session::lock::DetachLock`
locks a desktop the Linux Pier created (§10); then the hold is watched. A session that
is still the Deck's keeps the hold, whether it is locked, taken off the console or open, and a held agent stops
proving the desk's input hooks while its lock screen is up
(`DeskInputProof`). Windows has no unlock without the owner's password,
and a resume carries none (`UnlockAuthority::OwnersPassword`), so a
returning Deck that finds its session locked is refused (resume code
`unsupported`, "The session was locked when the connection dropped. Sign
in again to unlock it.", `HeldResume::SignInAgain`) and the hold
continues. The Deck asks its user for the password, and that sign-in
unlocks the same session through the credential provider and takes the
held desktop over, as a same-account sign-in always has: the session, its
applications and its windows are as they were, and the new attachment
gets a resume grant of its own. A Deck that comes back to a Windows Pier
after a lost connection therefore asks for the password; a Linux Pier
unlocks the console on resume. Taking the held session off the console
instead, so that a resume could put it back without a password, remains
GAP-044's route.

Under `privacy.desk = "required"`, and for a session that is already
locked when its Deck leaves, the hand-back locks first (`hand_back_order`
gives `DETACH_STEPS`): the agent stops streaming and stays, keeping the
desk dark with its input blocked, until the session is locked; only then
does it exit, and the displays Windows does not let a locked session
restore are restored behind the lock screen by `restore-display
--console`, LocalSystem on the console's input desktop (the secure
desktop). A refused lock follows the shared wait (`WaitingDesk::Required`):
asked again every `LOCK_RETRY` for `UNLOCKED_GRACE`, then every
`HELD_LOCK_RETRY`, with no end. The desk is never given back unlocked; the
first refusal is a warning and the hold an error at every log level, and a
Deck that signs in meanwhile is told why the PC is busy. The agent holds
to the same rule on every other way out (a desk check that refused the desk
while streaming, an error, a broker that went away): it locks its own
session and confirms the lock before it restores the desk or releases the
hooks, on the same schedule. A hand-back whose agent had already gone keeps
asking too. Ctrl+Alt+Delete still reaches Windows, so a person at a dark
desk locks the session there (Lock) and signs in again as usual. A Pier that
stops, or a Windows that shuts down, asks once and does not wait (the
shutdown budget); the session guard then locks after the displays come
back. An agent that has failed cannot hold the desk: its watchdog restores
the displays and the session is locked after, as before. `"on"` and
`"off"` keep the outputs-first order (PD-033).

### 5. Privacy while a Deck is attached

- **Monitors:** every CRTC's gamma ramp is set to zero through RandR (the
  original ramps journalled first) and proved by reading it back; NvFBC and
  XShm read the framebuffer before the ramps, so the Deck's picture is
  unaffected (lab: identical decoded frames with and without the blank).
  Re-asserted every second (a colour daemon resetting a ramp, a monitor
  turned on).
- **Input:** every evdev device a person can drive the desktop with
  (keyboard, mouse, touchpad, tablet, touchscreen) that is a real device on
  `seat0` is grabbed (`EVIOCGRAB`), including any device udev gives keys
  (`ID_INPUT_KEY`); Arcen's own virtual devices, another product's virtual
  devices and a Deck's bridged tablet (recognised only by the tag and
  `LIBINPUT_IGNORE_DEVICE` Arcen's own udev rule sets, never by a device's
  own name or serial) are never touched
  (`console_session::disables_while_attached`). Re-scanned every second for
  hot-plugged devices. The kernel releases a grab when its descriptor
  closes, so a crash gives the desk its input back.
- **Sound:** the session's audio lease mutes the desk's speakers while
  attached (`audio.local_playback`), and only then: it ends with the
  hand-back and is taken again (and proved, or the returning Deck is
  refused) before a resumed Deck is shown anything.

*Amended 2026-11-18 (after 0.20.3): desk privacy is the default on every Pier.*
Nobody should be able to look over the shoulder of someone working through
Arcen. One shared policy decides it for every Pier
(`arcen_session::console_session::desk_privacy::DeskPrivacyPolicy`, the
common configuration key `privacy.desk`):

- **On unless turned off.** `privacy.desk` absent or `"on"`: while a Deck is
  attached to a desktop the desk's own monitors show
  (`DeskExposure::PhysicalDesk`: a joined console, another product's desktop
  in guest mode, an Arcen login session, the macOS console owner's Aqua
  session, the Windows console session), the desk's monitors are dark and
  its keyboard and mouse blocked. `"off"` keeps the desk live; it is an
  explicit administrator choice, logged at every Pier start
  (`DeskPrivacyPolicy::startup_line`, a warning when off). A desktop no desk
  monitor shows (a headless Wayland desktop) has nothing to protect
  (`DeskExposure::NotShown`).
- **Fail closed and visible.** *(Since 2026-11-24 only with
  `privacy.desk = "required"`; see the amendment at the end of this
  section.)* A desk that cannot be made private refuses
  the attachment with the shared `console_privacy_unavailable` code (and the
  cause, `DeskPrivacyRefusal::deck_reason`), never streams it unprotected.
- **The Deck is told.** The hello carries `desk_privacy` (`blanked`, `off`,
  `not_shown`); every Deck shows one line from the shared
  `arcen_deck_core::session_truth::desk_privacy_note`: "Host screens are
  blanked for privacy", or that desk privacy is turned off on the host.
- **Restored exactly; order unchanged.** Detach, a Deck that disappears and
  a Pier or launcher that dies are handled as before (§4, `DETACH_STEPS`,
  the journals, the lock guard): lock first, then monitors, sound, input.
  With `"off"` the hand-back still locks first, and a session that will not
  lock is made private again (Linux `ConsolePrivacy::show_desk` ends at the
  hand-back).
- **How each OS makes the desk dark is earned (PD-027).** Linux zeroes every
  CRTC's RandR gamma ramp (journalled, restored byte for byte, also on
  another product's desktop) and grabs the
  seat's evdev devices. macOS zeroes each display's transfer table and runs
  an event tap that passes only the Pier's own events. Windows cannot show
  a black ramp (GDI's `SetDeviceGammaRamp` silently refuses ramps that would
  make the screen unreadable, such as all black, per its documentation), so
  the Windows Pier **detaches the desk's monitors** instead:
  1. The session is served on a display no monitor at the desk shows: a
     virtual display on the same GPU (an indirect display whose adapter
     software enumerated, `\\?\ROOT#…`, and whose EDID names a virtual
     display driver that shows its display nowhere: the Pier's own `ARN`,
     or the GeForce guide's Virtual Display Driver `MTT`; screen extenders
     that show the desktop on a tablet at the desk are not trusted) is
     chosen over a configured desk monitor
     (`deskside::virtual_output_instead_of`), or the NVIDIA head the Pier
     provisions with nothing physically connected.
  2. Once the display lease holds, every other output is detached from the
     desktop with `SetDisplayConfig` (`DisplayLease::detach_desk_monitors`),
     a mutation under the lease's armed recovery journal
     (`C:\ProgramData\Arcen\runtime\display-recovery.json`), exactly as an
     exact-isolated single display always was.
  3. It proves that no lit output has a monitor physically connected behind
     it (`deskside::prove_desk_dark`, shared `desk_outputs_dark`; NVAPI must
     have enumerated the GPU's displays, or the output counts as unknown; a
     virtual GPU in a guest drives no desk monitor) and repeats the whole
     proof on every health tick. *Amended 2026-11-23:* each repeat is judged
     against the outputs the passing proof lit (shared `recheck_dark_desk`).
     It ends the session at once when a lit output has a monitor behind it
     (`desk_monitor_lit`), or when any output is lit now that was not lit
     then: `desk_monitor_unknown` when it cannot be described (an integrated
     GPU, a USB display adapter), `desk_outputs_changed` otherwise. Only a
     desk that could not be read whole, and lights nothing the proof did not,
     is proved again: a failed topology or device read, nothing lit, a proved
     output that cannot be described this moment (an NVAPI call failed, for
     example), or only some of the proved outputs lit (one dropped out during
     a mode set), as while a driver re-detects its connectors. Such a repeat runs every 400 ms for
     up to 2 s (`DESK_PROOF_SETTLE`), never past the in-flight limit (3 s,
     `DESK_PROOF_IN_FLIGHT_LIMIT`), and within one budget of 6 s of repeats
     for the whole attachment (`DESK_SETTLE_BUDGET`); once that budget is
     spent the next such failure ends the session at once.
  4. The desk's keyboard and mouse are blocked by low-level hooks that pass
     only Arcen's marked input (proved live by the keyboard canary; the
     mouse hook is installed but not proved live, GAP-051).

  Restore: the lease's restore puts back the exact saved topology when the
  Deck leaves (outputs first, then the lock, `DETACH_STEPS_OUTPUTS_FIRST`);
  if the session agent dies, its out-of-process watchdog
  (`display::spawn_display_recovery_watchdog`, `WATCHDOG_RESTORE`) replays
  the journal and the broker locks after it (`hand_back_session`,
  `await_display_restore`); if the service dies, the journal is replayed by
  the next session's acquire or by `console_recovery` at service start and
  on the next console change (`arcen-pier restore-display --console` as
  LocalSystem on the console's input desktop). With `"required"` (since
  2026-11-24; best effort otherwise) a desk is refused only where
  detaching fails (`desk_detach_failed`, rolled back) or a monitor at the
  desk would still show the session (`desk_monitor_lit`, `desk_monitor_unknown`:
  no virtual display and the NVIDIA head is a desk monitor, or another
  vendor's GPU NVAPI cannot describe), and the agent locks the session
  before the desk's input comes back. Open (GAP-050): between a sign-in
  that unlocks or moves the session to the console and the detach (the
  agent's start and display lease, a few seconds) the desk's monitors show
  the session; on an agent crash the restored monitors show it unlocked
  until the broker's lock.

*Amended 2026-11-24 (0.21.3): desk privacy is the Deck
user's choice, kept as far as the host can, and fail-closed only where the
administrator requires it.* A Windows Pier on a GeForce that needs DisplayPort
dummy plugs (the standard way to run such a card headless) refused every
connection: NVAPI reports the plug as a physically connected display, so the
desk could never be proved dark (`desk_monitor_lit`), and a session that did
start was ended when a re-proof failed. Dummy plugs must work, and a Pier that
cannot darken a display must not disconnect people for it. This supersedes
the two bullets above ("On unless turned off", "Fail closed and visible")
where they differ:

- **Three administrator values** (`DeskPrivacySetting`, resolved by
  `DeskPrivacyPolicy::resolve` into a `DeskPrivacyMode`):
  - `"on"` or absent (`FollowDeck`; every installer ships `"on"`): each
    Deck's user chooses. The Deck sends its choice at authentication
    (`AuthResponse.desk_privacy_request`: `private` or `visible`; absent, as
    from an older Deck, or unknown is `private`). Private is the Deck's
    default.
  - `"required"` (`Required`): always private whatever the Deck asks, and
    fail-closed exactly as before this amendment: a desk that cannot be made
    private refuses the attachment, and a re-proof that fails ends it, with
    `console_privacy_unavailable` and a reason that names the administrator's
    requirement and the likely cause (a dummy plug or a monitor that cannot be
    darkened). For sites whose security rules demand it.
  - `"off"` (`Off`): never private, as before.
  Any other value fails `validate-config`.
- **The plan** (`DeskPrivacyPolicy::plan(exposure, deck)`): a desktop no desk
  monitor shows is `NotShown`; otherwise `"off"` is `OperatorOff`,
  `"required"` is `Required`, and `"on"` is `BestEffort` when the Deck asked
  for privacy and `DeckVisible` when it did not.
- **Best effort.** Under `BestEffort` the Pier applies every measure it can
  (blocks the desk's input, darkens or detaches every display it can) and
  records what it could not (`DeskPrivacyState::attach_failed`, split into
  screens and input by `DeskPrivacyRefusal::part`). A shortfall is a warning,
  never a refusal, a disconnect or a crash. Mid-session re-proofs (the
  Windows `DeskWatch`, the macOS display watch) downgrade the same way
  (`DeskPrivacyState::watched`): the session continues, the Pier logs one
  warning per change, and a desk proved dark again is reported as recovered
  (the Windows watch keeps proving after a failure, `DeskWatchOnRefusal::Keep`,
  at most once per `DESK_FULL_PROOF_INTERVAL`). A console that another
  account took over still ends the attachment: that is not a privacy failure
  (§4).
- **The Deck is told precisely.** The hello's `desk_privacy` gains
  `required`, `visible` (the Deck's user turned it off), `screens_visible`
  (the screens could not all be darkened), `input_live` (the desk's input
  could not be blocked) and `not_private` (neither); a change during the
  session is sent as `desk_privacy_update`. Older Decks ignore tokens and
  messages they do not know.
- **The session's own outputs are never taken away.** Every output the
  served topology is captured from (the single served display, or each
  Match My Layout monitor: on a GeForce with dummy plugs, each plug carries
  a stream) is the session's own (`DeskOutputRole::Served`,
  `desk_output_measure`): desk privacy never detaches, disables or powers it
  down, and acts only on the other outputs (`desk_outputs_to_hide`; the
  Windows Pier supplies the served device names and detaches nothing when
  only they are lit). A monitor behind a served output is the known
  shortfall (`desk_monitor_lit`: `screens_visible`, a refusal when
  required). Linux and macOS darken a served output only through its gamma
  ramp or transfer table, which capture reads before, so the stream is
  untouched.
- **Each Pier's adapter stays thin.** Windows keeps the hooks and the detach
  when the proof fails; macOS runs without its event tap when the tap cannot
  be installed or would drop the Deck's own virtual HID input, and keeps
  darkening every display it can; a joined Linux X11 console keeps whatever
  of its grab and gamma blank succeeded; a joined Wayland session shown on a
  desk monitor is served with `screens_visible` (a Wayland desk still cannot
  be blanked from outside its compositor). A joined Linux console re-asserts
  its blank and grab every second; each part's outcome is recorded
  (`DeskPrivacyState::watched_part`), the launcher reports a change on its
  console link (`desk_privacy` line) and the stream tells the Deck
  (`desk_privacy_update`). Under best effort a failure there keeps the
  session; under `required` it ends the attachment as on Windows and macOS:
  the launcher says `desk_privacy_lost` with the cause, hands the console
  back locked, and the stream closes with the shared reason
  (`DeskPrivacyRefusal::deck_reason`). A stream subscribes to these changes
  before its hello is built, so a change between the two is never missed.
  A grab that fails on one input device still grabs every other one. Each
  returning Linux Deck (a resume, or a new sign-in to the held session)
  sends its own choice with `attach`; the launcher re-plans the console
  (`ConsolePrivacy::replan`), makes it private again if this Deck asks, and
  answers with the status its hello carries. A resumed Windows attachment
  keeps the choice its sign-in made.

### 6. Displays and layout

*Amended 2026-11-17: per-monitor streams and the Deck's layout replace
"one picture of the whole X screen" for a Deck that matches its layout.*

**A single-display Deck** (or Primary display only, or a windowed Deck) is
served the whole X screen as one picture at the console's own size, and the
Deck scales it (`capenc_stream::fit_whole_screen` above the encoder's limit:
NVENC H.264 takes 4096 pixels a side, H.265 and AV1 8192). The console keeps
its layout, except on a host with the installer's forced virtual outputs
(below), whose first output takes the Deck display's size.

**A Deck that matches a layout of several displays** gets one stream per
monitor, one window per monitor, presented in step on one timeline
([ADR 0026](0026-one-timeline-for-every-monitor.md)), as the reference
products do (one encoder or one capture per head). The Deck's layout is the
authority while it is attached:

1. The Pier offers `multi_monitor_v1` from the GPU's heads (configured or
   discovered, at most four) and plans the Deck's topology with the shared
   planner (positions translated to a non-negative origin, rotation, scale),
   backend `console_outputs` (`physical_outputs` on the wire).
2. The launcher, after the console is private and before the session is
   unlocked, lays the plan on the console's own outputs
   (`arcen_outputs::console_layout`): the i-th monitor on the i-th connected
   output in RandR order, at its size, place and rotation, the Deck's
   primary as RandR primary, unused outputs off. An output that lists the
   monitor's size drives it exactly; otherwise it keeps a mode it offers (a
   desk monitor's own panel timing) and scales through `ViewPortIn`, so a
   monitor is never driven with a timing it did not offer. The layout is
   one NVIDIA MetaMode over NV-CONTROL, applied atomically and proved
   through RandR (every head's CRTC; the active outputs exactly the plan's,
   in order, because capture addresses them by that position). Any CRTC the
   layout lights is blanked at once.
3. Each monitor gets its own capture (NvFBC `TRACKING_OUTPUT`, or XShm for
   Grading) and encoder, and its stream on the video lane; region input
   lands on the output that shows the region.
4. Before anything changes, the launcher journals the console's own layout
   (`/run/arcen/console-layout.json`: MetaMode, RandR primary, outputs it
   hid). The hand-back (§4) restores exactly that after the session is
   locked and while the monitors are still blank, then the ramps, then the
   input. A resume lays the Deck's layout again. A launcher or Pier that is
   killed leaves the journal; the Pier's console recovery locks the
   session and restores the layout, then the monitors.
5. A console that cannot take the layout (fewer connected outputs than
   monitors, a refused or unproven MetaMode) is put back as it was. A Deck
   that matches its layout never takes a subset, so the sign-in ends with
   "Match My Layout could not be negotiated with this host: its console
   cannot take this layout." The Deck recognises this and offers Primary
   Display Only, which is served as one picture. The Pier logs the typed
   reason (`console_outputs_insufficient`, `console_layout_refused`, …).
6. A launcher that is killed usually leaves the login screen in front of
   the session, and NVIDIA applies no mode while its X server is off the
   active VT. The Pier keeps the layout journal and restores the console's
   own layout as soon as the session is on screen again. If the session
   ends first, the journal is dropped with it.

The scale stays the user's: GNOME on X11 has one integer scale for the whole
screen and the person at the desk uses it too, so Arcen does not change it.
The refresh follows the output's mode for the size (a MetaMode names a size,
not a rate).

**A headless console** (a virtual GPU, a rack workstation) has one connected
output. `install-arcen-pier --console-outputs N` writes an `xorg.conf.d`
`OutputClass` (matched on the NVIDIA kernel driver, so it merges into any
site `xorg.conf`) that forces N NVIDIA heads connected with only the first
lit; the X server reads it at its next start. Unused forced heads are hidden
(RandR `non-desktop`) while a Deck is attached. `--no-console-outputs` and
`--uninstall` remove it. This changes the host's display configuration and
is never done without the flag (Release/Security review).

**Two Decks.** One Deck per Pier (the admission gate); a second sign-in of
another account is refused as in §7, so the owner's layout is the one
applied.

**Whoever connects gets their own layout.** *Amended 2026-11-25.* A Deck
that matches its layout is laid out to its own monitors on every
connection, whatever an earlier connection left, on every Pier: a fresh
sign-in, a sign-in that finds the account's session held for an earlier
Deck, the sign-in hand-over, a console Arcen owns and one another product
owns (§8). Native modes only on another product's desktop, never
while its client is attached, and the Windows Pier still puts the desk's
own layout back when the Deck leaves (PD-043).

Which layout a console is asked to take is shared
(`console_session::connect_layout::console_layout_ask`): several monitors
always; one display on the installer's forced heads (above) and, for a
Deck that matches its layout, on another product's desktop, which an
earlier client or Deck left in its own layout. Before, a one-display Deck
there was shown the output its primary mapped to, at the earlier layout's
size. A console at a desk keeps its own layout for one display, as it has
after every earlier Deck.

Nothing an earlier connection left decides it. A sign-in never reattaches
a console held for an earlier Deck: the same account's sign-in takes the
held session over (`direct_reconnect::fresh_sign_in`), which hands the
console back (`detached_hold::after_deck_left`), and the sign-in joins it
afresh. A resume is the attachment it resumes: its grant is bound to the
whole layout request (each Pier's resume `TopologyBinding`), so a Deck
whose layout changed signs in afresh. A layout journal only records what
to put back. The Windows Pier gives every sign-in an agent of its own,
planned from that sign-in's request; a same-account sign-in drains a held
one first, and a stale display journal is replayed before anything
changes. The macOS Pier creates each connection's displays and has no
resume. A joined GNOME Wayland console (§9) is not laid out to a Deck yet
(GAP-042).

### 7. Refusals

The Deck shows the code first, then the reason; the Pier logs the code as
`SESSION_AUTH_FAIL` `reason_class` (stage `desktop`) at every log profile.
The codes and what an administrator does about them are in
[error codes](../operations/error-codes.md#linux-sign-in-refusals):
`console_other_user`, `console_other_user_other_product`,
`console_wayland_session`, `console_no_session`, `console_sessions_unreadable`,
`console_sign_in_unavailable`, `console_sign_in_failed`,
`console_session_unreachable`, `console_privacy_unavailable`,
`console_sign_in_other_product` (§8). A sign-in of
another account while a joined console waits for its Deck's resume gets the
same `console_other_user*` refusal.

### 8. Coexistence with other remote-desktop products

Some workstation remote-desktop products start their own display manager
and X server on the console's GPU (their own PAM service, their own Xorg on
another VT). Arcen recognises them generically: a `seat0` session whose PAM
service is not the display manager's.

- The account's own session in such a product is joined as that account.
  Arcen never starts a second X server on the GPU beside it, and never stops
  or reconfigures the other product.
- Another account's session in it refuses with
  `console_other_user_other_product`, even when it is not in the
  foreground (the display manager cannot switch it away).
- While such a product is the host's display manager there is no GDM login
  screen, so a remote-first sign-in through GDM is not possible there
  (`console_no_session`), and GDM cannot resume its session. That is the
  other product's design, not something Arcen works around. (A host whose
  only "display manager" is such a product's own unit aliased as
  `display-manager.service` counts as having one.)
- A dedicated desktop (§10) refuses `seat_occupied_other_product` instead
  of starting beside such a desktop on any VT.

*Amended 2026-11-17 (0.20.2, from a field workstation whose desktop another
product owned):*

- **Outputs come from the live X server.** The console's multi-monitor offer
  and plan are read from the console's own X server for each connection
  (`console_layout::live_console`), not from a probe X server started at Pier
  start: that probe hangs while another X server holds the GPU and so
  withheld the offer. No probe starts while any X server runs. The pre-auth
  read is cached for 5 s, and the display is opened only after its
  authority file (no link followed, proved on the opened file) and the X
  socket's peer (root, or the user's own server with the user's own cookie,
  shared `x_server_peer_trustworthy`) are proved.
- **Guest mode.** Such a desktop is shown as it is: Arcen never lays it out
  (no MetaMode, RandR or non-desktop change). A Deck that matches its layout
  is served one stream per output the desktop shows, each captured from its
  output by RandR name (capenc `output=<name>`). Outputs forced connected
  without a mode never take a monitor (shared `takes_a_monitor`).
  *Amended in the 0.20.2 re-issue:* the Deck's monitors had to have exactly
  the desktop's layout (sizes, rotation, relative places), else
  `console_layout_differs` and Primary Display Only. The other product lays
  the desktop out for its own client at connect (its client's resolutions,
  positions and primary; it requests no restore at session end), so a Deck
  on the same computer differed by scale, refresh or placement and fell
  back. The Deck's monitors are now mapped by arrangement (shared
  `match_shown_layout`): the Deck's primary on the desktop's primary, each
  further monitor on the output nearest its own direction from the primary,
  places measured in each primary's size so another scale maps the same.
  Each mapped output is served at its own place, extent, mode and rotation
  (`console_layout::GuestLayout::fit`: the committed plan keeps the Deck's
  logical geometry, so its pointer maps proportionally, and the absolute
  pointer spans the whole X screen); the Deck fits each picture into its
  window, letterboxing only when the shapes differ. More outputs than Deck
  monitors: the nearest are shown, the others named as not shown; fewer:
  the Deck monitors left over get no window (the offer to a guest desktop is
  therefore the full monitor count). The applied topology declares the
  shown outputs' bounding box, which need not start at the screen origin.
  `console_layout_differs` remains only for a desktop that shows nothing.
  The mapping and each difference are logged by the Pier and named by the
  Deck (`arcen_deck_core::multi_window::layout_note`). A single-display
  Deck is shown the output its primary maps to by the same rule
  (`arcen_outputs::console_layout::single_display_output`), captured by
  name at its own size, its absolute pointer and pen placed on that output
  of the whole screen (`ShownOutput::screen_point`); the whole screen in one
  picture only when one output is shown (amended 2026-11-18, field report).
- **The Deck's layout on another product's desktop.** *Amended 2026-11-25
  (field: a workstation whose desktop another product owned; customer
  decision).* The product lays the desktop out for its own client at each
  connect and leaves that layout when the client goes. A Deck that attached
  later was mapped onto the last client's layout: a user working from home
  on 16:10 monitors (2560×1600 and 1920×1200 on its left) found the office
  layout (3840×2160 and 2560×1440) and got black bars on both monitors.
  Arcen now does what the product does:
  1. **On attach, while the product has no client attached**, a Deck that
     matches its layout lays the desktop out to its monitors with the §6
     route (one NVIDIA MetaMode over NV-CONTROL, the Deck's primary as
     RandR primary, proved through RandR), journalled first in
     `/run/arcen/console-layout.json`, but **never scaled**
     (`arcen_outputs::console_layout::plan_native_console_layout`,
     `HeadScaling::NativeOnly`): each head drives a mode of exactly its
     Deck monitor's size, one the output lists (the product's own
     client-sized modes included) or one Arcen adds (a VESA CVT
     reduced-blanking timing at the Deck's refresh, at most 60 Hz,
     `RRCreateMode` named `<W>x<H>_<Hz>_arcen` and `RRAddOutputMode`; kept
     on the X server as the product keeps its own); every head states an
     identity `Transform` in the MetaMode, and the proof reads each CRTC's
     transform back. Outputs the layout does not use are switched off by
     the MetaMode and never hidden (`non-desktop` is set only on the
     installer's own forced heads of a console Arcen puts back). *Field,
     2026-11-25:* a hand-applied scaled head
     (`ViewPortIn` 2560×1600 in a 3840×2160 mode, a RandR transform of
     0.667 by 0.741) made the guest mapping read the output at its mode
     size and refuse the Deck, and the product kept that transform on the
     CRTC when it next laid the desktop out (a 1920×1200 monitor shown in
     1280×889). The mapping by arrangement now reads each output as the
     area its CRTC covers in the X screen (`shown_size`: RandR's CRTC
     size, after any transform), which is what its capture delivers, so a
     desktop someone else scaled is shown rather than refused (the Deck
     was told "its console cannot take this layout", and Primary Display
     Only failed with "capture/encoder initialization failed", because the
     output's mode reached past the screen's edge). The decision is shared
     (`console_session::guest_layout::guest_layout_policy`) from three
     facts: the console's owner (`ConsoleOrigin::OtherProduct`), whether
     the product's client is attached (`ForeignClient`: Attached, Absent,
     Unknown; on Linux the user-space input rule below, looked at again
     just before the layout) and `platform.session.guest_layout` (`deck`,
     the default, or `keep`). Only a proved absent client and `deck` give
     `ApplyOnAttach`; Attached or Unknown give `Shown` whatever the
     setting. A resume lays it again under the same rule (the returning
     Deck is refused while the client is attached).
  2. **Never while that client is attached, or when Arcen cannot tell.**
     The desktop is then shown as it is (the mapping by arrangement
     above). In practice the join is already refused
     `console_in_use_other_product` then; the second look narrows the
     window between the join and the layout.
  3. **Disconnect leaves the layout**, as the product does
     (`console_session::guest_layout::layout_departure` is `LeaveDecks`
     for another product's desktop, `RestoreOwn` for every other console).
     The hand-back still locks the session first, gives the monitors their
     ramps, the sound and the input back, and never ends the session. The
     journal becomes the record of the layout before
     (`/run/arcen/console-layout-before-deck.json`: MetaMode, primary;
     the Pier logs the join with `layout=applied`); nothing restores it.
     A launcher that dies leaves the same: the Pier's recovery locks the
     session and keeps the layout. The journal says the layout stays only
     once it is proved and settled; until then, and when putting it back
     fails, the recovery puts the layout before back. Putting the old
     layout back would undo what the user just had and, while the
     product's client connects, fight that product's own layout.
  4. **A desktop that cannot take it is put back and shown.** Too few
     outputs, an output that will not take a mode of the Deck's size
     (`console_layout_no_native_mode`, naming the output and the mode the
     driver refused), a MetaMode refused or not proved (a transform left
     on a head included): the journal is restored at once (as §6), the
     reason is logged, and the Deck is served by the mapping above rather
     than refused.
  5. **What stays shown as it is:** `platform.session.guest_layout =
     "keep"`, a single-display Deck that does not match its layout
     (Primary Display Only, windowed; the console's own §6 rule for one
     picture is unchanged), and a desktop another product started from a
     text console (`startx`, also `OtherProduct`) only through the
     setting. A one-display Deck that matches its layout is laid out to
     that display (§6, "Whoever connects gets their own layout").
  Routes considered: keep "shown as it is" and let the Deck fit (the
  field's black bars); apply and restore on departure (the previous
  attempt, 0.20.2: restoring under a product client that is connecting
  fights its layout, and a user who comes back through Arcen finds the
  other layout again); per-Deck virtual outputs added to the product's X
  server (NVIDIA cannot add heads to a running X server Arcen does not
  configure); detecting the product's client from its own session files
  or ports (product-specific; the kernel-grounded input rule below
  already holds for every product and fails closed). The Windows Pier
  lays out the console session's displays and puts them back when the
  Deck leaves, and another product's client there holds a session of its
  own; the macOS Pier adds displays of its own (PD-043).
- **One remote product at a time.** While that product's client is connected
  the sign-in is refused `console_in_use_other_product`; when it connects
  during an attachment the attachment ends with that code. A connected
  client shows as a keyboard or pointer made in user space (shared
  `another_products_client_input`): a udev keyboard or pointer without a
  parent device, neither on the host-platform bus (`BUS_HOST`) nor a
  Bluetooth device at the desk (`BUS_BLUETOOTH`: BlueZ makes Bluetooth LE
  keyboards and mice through uhid), not Arcen's (its vendor) nor a Deck's
  bridged device, while a process outside Arcen holds `/dev/uinput` or
  `/dev/uhid` open (a device made through them lives only while its
  creator holds one; unreadable counts as such a process).
  *Amended in the 0.20.2 re-issue:* "any parentless device that is not
  Arcen's" also took a firmware driver's device; a field workstation's
  `HP WMI hotkeys` (hp-wmi registers it without a parent, on `BUS_HOST`,
  with a few keys and switches) refused every join with no client
  connected.
  *Amended for 0.21.2 (field report):* a colour-grading panel's service
  holding `/dev/uinput` for the panel's own keyboard and mouse refused every
  join and ended live attachments. Two rules narrow "a process outside
  Arcen" without letting a remote product hide. (1) Only a holder of the
  interface that made the device counts (shared
  `UserSpaceInputInterface::of_device`, `HeldInterfaces::could_make`): a
  uinput device has no parent, a uhid device sits under the uhid misc
  device, and each lives only while its interface is held; so bluetoothd,
  holding `/dev/uhid` for the desk's Bluetooth keyboard, no longer makes the
  panel's uinput keyboard another product's. (2) Drivers for hardware at
  the desk (`LOCAL_INPUT_DRIVERS`: the panel service, bluetoothd, a mouse
  configuration daemon and a key remapper) are not creators, but only when
  the name matches exactly, every user ID of the process is root's, and the
  running file (`statx` through `/proc/<pid>/exe`) is a regular file root
  owns and only root can change (shared `local_input_driver`). A tool other
  programs can type through (input-automation daemons, remappers with a
  text or macro command, a game client with remote streaming), a driver
  that loads user plugins, and an interpreted daemon (its executable is the
  interpreter) are never listed. The research is in the 0.21.2 pull request.
  When another product starts a desktop of
  its own while a Deck is attached to any other console session, the
  attachment ends with `console_other_product_started`.
- **Input devices on any console display.** A joined console's display may
  have any number (the field's was `:100`); its devices are named with
  `SessionInputIdentity::for_joined_console`. Before, uinput was refused for
  a display outside `:1`..`:99` and setup ended with a bare close.
- **The other product as display manager.** `display-manager.service`
  pointing at a unit that is not a display manager counts only while that
  unit runs (shared `ConsoleHost::from_display_manager`); stopped, the host
  is served as one without a display manager (§10).
- **No silent setup end.** From the moment a sign-in has its desktop, any
  exit before the stream starts records `SESSION_INTERRUPTED` (stage
  `setup`, the close the Deck was sent as reason class) at every profile
  (`net/setup_watch.rs`), and every close carries a reason.

*Amended 2026-11-24 (from a field workstation whose display manager was
another product running GDM headless):*

- **A product that runs the sign-in.** A product that is the running
  display manager may run GDM with XDMCP on and its local login screen off
  (`xdmcp/Enable` true, `xdmcp/ShowLocalGreeter` false, set in GDM's runtime
  settings while the product runs). GDM 40 then never starts its local
  display factory (`gdm-manager.c`: the factory starts only when XDMCP is
  off or `ShowLocalGreeter` is on): no login screen on `seat0` after boot or
  after a session ends. The Pier reads GDM's effective settings
  (`/run/gdm/custom.conf` per key over `/etc/gdm/custom.conf`, GDM's
  defaults beneath; `gdm3` paths too) only while another product's unit is
  the running display manager (shared
  `console_session::local_login_screen`, `ConsoleHost::login_screen`). A
  display manager's own alias (`gdm.service`…) is never affected.
- **Which switch counts depends on GDM's version**, which the Pier asks
  of GDM itself (`gdm --version`, or `gdm3`, bounded) on the same path.
  GDM 49 and earlier: the `[xdmcp]` rule above. GDM 50 removed XDMCP and
  `ShowLocalGreeter`, so its local login screen always runs and an old
  `[xdmcp]` pair withholds nothing. GDM 51 and later read
  `[daemon]` `ShowLocalGreeter` (default true) alone. When the version is
  unknown, `ShowLocalGreeter=false` in either group is treated as unproven
  (below); without it the login screen counts as shown.
- **Refused with the product named.** With nobody signed in such a sign-in
  is refused `console_sign_in_other_product`, naming the product as the
  host describes it (the display-manager unit's `Description=` when it is
  plain words, else the unit), never `console_no_session`. A login screen left on that
  console is never signed in through. The account's own desktop in the
  product is joined as before (guest mode), so signing in with the product
  once and disconnecting it gives Arcen that desktop.
- **No login screen is asked of that GDM.** A transient login screen
  (`CreateTransientDisplay`) of a factory that never started waits for
  virtual terminal 1 until the product's X server leaves its terminal, then
  every waiting one starts an X server at once and the survivor takes the
  GPU from the product's next X server. So a joined session that does not
  lock is kept private (monitors blank, the desk's input disabled) and its
  lock retried, never put behind a GDM login screen
  (`UnlockedHandBack::on`); the failure is recorded once at error level per
  session, not on every retry. Where GDM's settings cannot be read on such
  a host, or GDM's version is unknown and a `ShowLocalGreeter=false`
  appears, a sign-in is decided as before but no login screen is asked of
  GDM either.

### 9. Wayland GDM sessions

Not joined in this change: a Wayland console session or login screen is
refused visibly with `console_wayland_session`, and administrators set
`WaylandEnable=false` (as other console products require).

Research (2026-11-16): the route exists. A GNOME Wayland session exposes
`org.gnome.Mutter.ScreenCast` and `org.gnome.Mutter.RemoteDesktop` on the
user's own session bus — the interfaces GNOME's own Remote Desktop uses for
desktop sharing of an existing session (gnome-remote-desktop
`docs/configuration.md`, "remote assistance" mode) — and the greeter hook
of §3 is present in GNOME Shell 49 (`libshell-17.so` resources
`gdm/oVirt.js`, `gdm/credentialManager.js`) as in 40. Arcen already drives
both Mutter interfaces for its headless Wayland desktops. What is missing is
Arcen's side: discovery of the user's session bus, capture of the physical
monitors (`RecordMonitor`) instead of virtual ones, input through the
session's RemoteDesktop session rather than the Pier's own socket, and the
lock screen (GNOME closes desktop-sharing sessions when the screen locks,
per the same document). That is work package `console-wayland`. The GNOME 49
lab host has no working GDM login screen on its vGPU (no KMS connector), so
it is proved on hardware with a display.

Amended 2026-11-20 (S4a, the target and ownership contract): the account's
own Wayland console session is no longer an anonymous refusal. The console
decision names it (`ConsoleDecision::JoinWayland`, a `WaylandTarget` with
logind's id, owner, leader, lock and origin), and the Linux adapter
(`hosts/linux/src/session/console_wayland.rs`) proves its compositor
read-only before refusing: the session's `XDG_RUNTIME_DIR` must be logind's
`/run/user/<uid>`, a directory only the account owns; its `bus` socket must
be the account's, and the kernel's peer credentials must say the account
serves it; one connection must own `org.gnome.Mutter.DisplayConfig`,
`ScreenCast` and `RemoteDesktop`, and the process the bus names must be,
by `/proc`, the account's, in the account's slice and this session's
compositor (its scope, its `XDG_SESSION_ID`, or the account's display
session); the names must not move while they are read; and logind must
still describe the same session (owner and leader) afterwards. A user bus
does not accept root, so the Pier asks it through `busctl` running as the
account, with no groups and an empty environment, only after the kernel
has proven the socket. Nothing is started and no Mutter method is called.
`wayland_join::prove_compositor` and `decide_join` hold every rule; while
the Pier has no join adapter the outcome is
`WaylandJoin::AdapterUnavailable`, which keeps the visible
`console_wayland_session` refusal and logs the proof. A Pier that creates
its own Wayland desktops creates one only when logind proved the account
holds no session at the workstation (`decide_before_creating_read`,
`creating_step`, amended 2026-11-20): the account's own Wayland
session is identified and proven the same way and joined (S4b, below), its
X11 session refused `console_session_unreachable`,
and an unreadable logind `console_sessions_unreadable`. Another account's
session, a login screen, SSH and background sessions, Arcen's own
desktops (`TTY=arcen`, seatless or on their own seat) and a session on
another seat are not the account's workstation session and do not stop
it. A reconnect to the account's own Arcen desktop creates nothing and is
not refused; one whose layout would need a new desktop keeps the existing
one rather than tear it down for a refused creation.

Amended 2026-11-20 (S4b, the join): a proven Wayland session is joined,
on an Xorg host's console path and on a host that creates its own Wayland
desktops alike (`wayland_join::creating_step` gives `JoinConsole`; the
sign-in then goes through the console launcher). The decisions are shared
(`console_session::wayland_attach`); the Linux side only gathers facts and
acts (`hosts/linux/src/session/wayland/join.rs`, `launcher.rs`
`run_wayland_join`):

- The console agent runs as the account, on the session's own user bus
  (`/run/user/<uid>/bus`): it starts no bus, no compositor and no
  PipeWire. Before it uses the bus it checks again, from inside, that one
  connection serves every Mutter name and that the bus reports that
  connection as the proven compositor process of this account. Upstream
  Mutter 49.4 and 50.0 accept `ScreenCast` and `RemoteDesktop` sessions
  from any client of the session bus and tie each session to the caller
  that created it (`meta-dbus-session-manager.c`; `RecordMonitor` and
  the input notifications check the creator); no allow-list applies.
- It records the session's own enabled monitors with `RecordMonitor` on a
  `ScreenCast` session paired with a `RemoteDesktop` session, at the size,
  place and scale the session runs them (`joined_outputs`); nothing of
  the session's monitors is changed. Input and the clipboard go through
  the same RemoteDesktop machinery as Arcen's own desktops; the pen stays
  absent.
- The Deck is served only when capture, input and lock are all proved on
  the running session (`decide_serve`): every monitor announced a stream,
  the input target accepted keyboard, pointer and an absolute motion, and
  the compositor's connection is GNOME Shell's (which locks on logind's
  request) with locking allowed by the account's settings. Otherwise the
  sign-in is refused `console_session_unreachable`.
- Desk privacy is judged before anything is brought forward: a session
  shown on a monitor at the desk, or whose monitors cannot be read, is
  refused `console_privacy_unavailable` while desk privacy is required,
  before it is activated or unlocked (`wayland_attach::monitors_at_desk`).
  A Wayland compositor's monitors cannot be blanked from outside without
  changing its state, and Mutter stops painting a monitor it powers down.
  A headless session's own virtual monitors show nothing at a desk. The
  desk's input is then disabled at the kernel. A monitor enabled, disabled
  or reconfigured during the attachment ends it (the recorded set and the
  privacy judgement were made for the old one); the session continues.
  The agent judges each change as Mutter signals it
  (`DisplayConfig.MonitorsChanged`, not a periodic check;
  `wayland_attach::monitor_change_step`): when the session now shows, or
  may show, on a monitor at the desk, the agent locks it through logind
  at once, as the account, before the attachment winds down, and the
  hand-back locks and checks again. Mutter often closes the sharing
  first (a recorded monitor that moves or loses its place closes its
  stream before `MonitorsChanged` is emitted), so the monitors are judged
  again whatever ended the attachment. Mutter reports a change only after
  it has applied it, so a monitor plugged in at the desk can show the
  session from Mutter's first frame on it until GNOME Shell's lock shield
  covers it: one signal delivery (`MonitorsChanged` or the sharing's
  `Closed`), one `GetCurrentState` and one `LockSession` call on
  already-open buses, then the shield. No
  compositor interface announces a monitor before it is shown, and
  blanking it from outside would change the user's session; the vGPU lab
  has no connector, so this window is not measured on hardware.
- A join is journalled (`/run/arcen/console-wayland-join.json`) before the
  desk's input is taken, so a Pier that dies mid-attachment locks the
  session when it starts again (the desk's input, held by the process
  that died, is already back). A sign-in the
  Pier switched to joining is served only that session: it never falls
  back to another console route, and never replaces the account's Arcen
  desktop.
- GNOME Shell closes screen sharing and refuses new sharing sessions
  while the session is locked (`js/ui/sessionMode.js`, Mutter's remote
  access controller). A locked session is therefore unlocked through
  logind after the account's password was proven, as an X11 session
  behind its login screen is brought forward, and refused if it stays
  locked. When the Deck leaves, the session is locked, the desk gets its
  input and sound back, and the attachment ends: it is not held for a
  resume (the lock closes the sharing), and the returning Deck signs in
  again and joins the same session. The session is never ended.
- Still refused, with evidence: a Wayland login screen. GDM 47 (RHEL 10)
  has no supported route for a third-party product to complete a sign-in
  into, or hand over to, a session through its Wayland greeter: it moves a
  remote id on reauthentication only for GDM's own remote displays
  (`gdm-manager.c`), and `CreateUserDisplay` exists from GDM 50 only.

Persistent per-user Wayland sessions, the resume hold and a desk with
physical monitors remain open (GAP-042).

### 10. Hosts without a display manager: a real login session

**Decision (amended 2026-11-17).** A host without a display manager
(`display-manager.service` not configured) is served like any workstation:
a Deck sign-in with nobody signed in starts the account's **real, persistent
login session**, the one a display manager would have started, and joins
it as a console session. It replaces the per-connection desktop as the
answer for this host class; `console_no_session` now means a configured
display manager whose login screen is not running (or no free virtual
terminal).

How Arcen signs the account in (learned from how display managers and
virtual-session products do it, nothing copied: a root process opens the
user's PAM session, starts the X server, runs the user's own X session
entry, and the session outlives the client connection):

1. The Pier authenticates the password and runs the account checks with
   its own PAM service, as for every sign-in.
2. The console launcher asks systemd to run `arcen-pier login-session` as a
   transient unit (`arcen-login-<uid>.service`, its log in that unit's
   journal), so the session belongs to neither the Pier nor the
   connection. That process opens a PAM session with Arcen's service
   `arcen-login` (`/etc/pam.d/arcen-login`, written by the installer from
   the host's own `login` stack: `auth pam_deny`, the host's `account` and
   `session` lines, `password pam_deny`; it authenticates nobody), with
   `XDG_SEAT=seat0`, `XDG_VTNR` the first free terminal from 7
   (`login_session::choose_vt`), `PAM_TTY=/dev/ttyN`, type `x11`, class
   `user`. `pam_systemd` makes it a logind session on `seat0` at that
   terminal (`Service=arcen-login`), and logind moves the process into the
   session's scope.
3. It starts the X server on that terminal from the packaged template
   (GPU, head, depth as for every Arcen X server), but as a console X server:
   the seat's devices are added and enabled (`with_console_input`), and
   Arcen's input devices reach it the same way they reach a display
   manager's X server.
4. It runs the account's X session entry (`/usr/share/xsessions`,
   `gnome-xorg.desktop` for `gnome`, `gnome-classic-xorg.desktop` for
   `gnome-classic`; only a plain command, `login_session::xsession_exec`)
   through the system's `/etc/X11/xinit/Xsession`, as the user, with the
   session's PAM environment and SELinux context: `gnome-session` with its
   settings daemons, autostart, desktop icons and a working "Log Out".
5. It waits until the user logs out (the session command exits) or the X
   server ends, then stops the X server and closes PAM: only the user's
   logout, an administrator (`loginctl terminate-session`) or a shutdown
   ends the session. A Pier restart or upgrade does not. The one exception
   is a session the launcher has just started and cannot make private (its
   display does not answer, or the desk cannot be blanked). Nobody has seen
   it yet and nothing could cover it at the desk, so the launcher ends it
   before giving the desk back.

From then on the session is an ordinary console session
(`ConsoleOrigin::Arcen`): the launcher joins it and makes the desk private.
**When the Deck leaves, the session stays private rather than locked.**
GNOME Shell creates its lock screen only when GDM answers on the system
bus (`LoginManager.canLock()` asks `org.gnome.DisplayManager` for its
version; GNOME Shell 40.10 `js/ui/main.js` creates `ScreenShield` only
then), so a host without a display manager has no lock screen, and a
session in the background is one virtual-terminal switch away. The
launcher still asks logind to lock it (a desktop with its own locker
honours that). If nothing locks it, the session is kept private: its
monitors stay blank and the desk's devices stay disabled in its own X
server, journalled and held by the Pier like any unsecured console (§4).
Shared `unlocked_hand_back` returns `KeepPrivate` for it: never "in the
background" and never "behind a login screen". It stays private until the
account signs in through Arcen again or signs out. A later sign-in of
the same account joins the same session (same processes). **One session
per account, one account at a time:** another account's sign-in is refused
`console_other_user` while it exists, in front or not, because a host
without a display manager has no switching between sessions and a Pier
serves one desktop. A domain account needs `arcen-login` weighed as an
interactive logon (the installer's SSSD snippet, §3 "Domain accounts").

*Amended 2026-11-18:* the session's head is created at the size of the
Deck display the first sign-in serves, with that display's generated EDID,
as a dedicated desktop's head is (the launcher passes the display's facts
to `login-session`, never EDID bytes; a display the generator refuses keeps
the template's EDID-less head, a multi-head request keeps the template's
heads); later sign-ins join it at that size (a joined console is never
resized, §6). The login keyring is unlocked with the sign-in's password
(§3, "The login keyring").

Known limits: without a display manager nobody can use the session at the
desk (there is no lock screen to unlock, see above), so it is resumed only
by an Arcen sign-in of its owner.

`platform.session.mode = dedicated` stays an **explicit administrator
opt-in**, never a default, for one technical reason: it is the only mode
whose X server's heads are created to the Deck's displays (one head per
Deck display, each the Deck's size, `match_layout`), which a joined
console cannot do until the console's own outputs can follow a Deck
layout (`console-multi-monitor`). It is per connection by design, and
documented as such.

The dedicated path is fixed where it stays:

- Before it starts an X server, any user's console session on `seat0` on
  any VT holds the seat (`x11_seat::x11_seat_holder`): refused
  `seat_occupied`, or `seat_occupied_other_product` when the display
  manager did not start it. A console logind cannot describe completely
  is refused `console_sessions_unreadable`, never taken for a free one.
- The X server's readiness is bounded (12 s) so a hung server fails inside
  the broker's open timeout; its log tail is recorded at error level and its
  `Xorg.log` and `xorg.conf` are kept in `<runtime_root>/last-failed/`.

**The headless Wayland provider** (ADR 0015, `linux-wayland-provider.md`,
hosts without an Xorg server such as Enterprise Linux 10) still creates
one desktop per sign-in for an account with no session at the workstation
(never beside one, §9). How long it lives depends on how its attachment
ends (corrected 2026-11-23: this paragraph said the desktop lives for the
reconnect window, which holds only for the first case):

- An attachment that negotiated resume (the Deck app) and lost its
  connection holds the desktop for the reconnect window
  (`auth.reconnect_window_secs`, 180 s by default); a resume or the same
  account's next sign-in rejoins it, and unresumed it is ended when the
  window closes. An explicit disconnect ends it at once.
- A failure on the host's side (capture or encode could not continue, a
  resize that could not be served) never ends it: the attachment ends, its
  display and the Pier's session slot are given back, and the desktop is
  kept, disconnected, for the account's next sign-in (amended 2026-11-23,
  `arcen_session::direct_reconnect::created_desktop_after_end`).
- An attachment that did not negotiate resume (the Deck CLI, older Decks)
  keeps the desktop, disconnected, however it ends; this was already so.

A desktop kept disconnected, in the last two cases, has no time limit of
its own, exactly like an Xorg desktop: it ends when its logind session
ends (logout, `loginctl terminate-session`, its compositor exiting) or the
Pier stops; a dead one is cleared at the next sign-in; and only when the
operator sets `platform.session.disconnected_idle_timeout_secs` (Linux
only, unset by default) is it torn down at the next sign-in after being
disconnected that long. Until then the same account's sign-in rejoins it
and another account's is refused as busy. A bound of its own is a
follow-up; it must not end a desktop the user still needs.

*Amended 2026-11-25 (a created desktop locks when its Deck leaves):* a
desktop the Pier created, Xorg or Wayland, used to stay unlocked after its
Deck left until the user's own idle lock (about 310 s on GNOME's
defaults), or for the whole of a resume hold. Every departure that leaves
it running now locks it at once, as a joined console is locked: kept for
the next sign-in, held for a resume, or kept after a host-side failure
(shared `desktop_session::lock::DetachLock::on_detach`). The Pier asks
logind (`loginctl lock-session`) and confirms `LockedHint=yes` while the
session slot is still the departing attachment's, so a newcomer's
authenticated unlock always comes after it. A same-account takeover is
locked too (the newcomer may yet be refused before its own unlock), and a
desktop that ends has nothing to lock. A session without a
lock screen (no GDM to unlock through, `LockPolicy::Disabled`) is not
locked: a lock there would be a lockout; that is logged as a warning. A
lock that is not confirmed is logged at error level with its reason
(`lock_refused`, `no_lock_screen`) and the desktop keeps running: a
created desktop has no desk to keep private, so `privacy.desk` does not
apply, and the next attachment still needs the account's password or a
resume grant. A resume unlocks the desktop again, as a sign-in does; on
Wayland the lock closes the sharing and the streams are recorded again
after the unlock (`desktop_session::remote_access`).

Making it persistent per user is tracked as `wayland-persistent-session`:
the same model applies (the user's GNOME session started through
`gnome-session` on the user's systemd manager, owned by logind, locked
on detach, joined by the next sign-in), but on a host without Xorg it
needs Mutter's headless session to outlive its launcher and the console
join over Mutter's ScreenCast/RemoteDesktop on the user's bus
(`console-wayland`, GAP-042), which do not exist yet.

### 11. Every failure is recorded at every profile

A sign-in that got no desktop records `SESSION_AUTH_FAIL` (stage `desktop`)
and an error line with its reason class; the launcher's own errors and
warnings keep their severity in the Pier log; a dedicated X server's failure
records its log tail and keeps its logs.

### 12. Audit: the Windows and macOS Piers against the principle

| Principle | Windows Pier | macOS Pier |
| --- | --- | --- |
| Joins the user's session | Yes: binds the account's WTS session against `WTSGetActiveConsoleSessionId` (`hosts/windows/src/windows_session.rs:1253-1262`) and moves the account's disconnected session onto the console (`:1671`, `WTSConnectSessionW`) | Yes: serves the console owner's Aqua session (`hosts/macos/src/console.rs:86-113`, `wait_for_console_owner` `:128`) |
| Remote-first creates the OS's real session | Yes: Credential Provider logon; Winlogon creates the console session the desk later sees (`windows_session.rs:1430-1446`, `first_login.rs:24`) | Yes, interactively: the login window is streamed and the user signs in there; cold boot with FileVault is not served (open, README) |
| Another account at the console | Refused, typed with the shared codes (`console_other_user`; `console_other_user_other_product` naming the session's WinStation listener and protocol when another remote-desktop service is attached; `windows_session::console_refusal`, `docs/architecture/windows-console-policy.md`) | Refused at once, led by the shared `console_other_user` code and naming the account at the console (`console::other_user_refusal`; `console.rs` tests `a_different_account_at_the_screen_is_refused_at_once_not_waited_out`) |
| Disconnect locks, never ends | Yes (amended 2026-11-18): a Deck that leaves for good gets the session locked (`LockWorkStation` as the user in that session, then disconnected from the console if it would not lock), outputs first (`DETACH_STEPS_OUTPUTS_FIRST`, PD-033), speakers last; a same-account takeover and a changed session are not locked (shared `Departure`). Amended 2026-11-25: a session held for its Deck's resume is locked at once (shared `held_lock::HOLD_STEPS`), and a returning Deck signs in again with its password, which unlocks the same session (GAP-044); under `privacy.desk = "required"`, or for a session already locked, the lock comes first and the displays come back behind the lock screen (shared `hand_back_order`, `lock_wait`) | Yes (amended 2026-11-18): a signed-in user's session is locked as its Deck leaves (the system's Lock Screen, `SACLockScreenImmediate` found with `dlsym`, proved from `CGSessionCopyCurrentDictionary`), then its displays and sound are given back (`DETACH_STEPS`); a console another account took over is left alone (shared `Departure`). The macOS Pier has no resume hold, so every departure is final. A person's **Stop Sharing** (macOS 14, `SCStreamErrorDomain` -3817; amended 2026-11-23) ends the attachment the same way, never restarted, and the Deck is told `capture_stopped` (shared `console_session::stream_end`, `Departure::CaptureStopped`); any other capture stop stays a capture failure. A refused lock is asked again for `UNLOCKED_GRACE` with or without desk privacy (shared `console_session::lock_wait`; a desk that was not private gives the slot to an authenticated Deck), and a failed console read never skips it. Under `privacy.desk = "required"` (amended 2026-11-25, §4) the desk stays private past the grace until the session locks, asked again every 30 s, and a Deck that signs in meanwhile is told why the host is busy Joining a locked session does not unlock it: the Deck signs in at the streamed lock screen (no public unlock after a password check) |
| Desk private while attached | Yes, by default (amended 2026-11-18, `privacy.desk`, §5): served on a virtual display or a head with nothing connected, the desk's monitors detached from the desktop (`DisplayLease::detach_desk_monitors`) and proved dark (`hosts/windows/src/deskside.rs` `prove_desk_dark`), the desk's keyboard and mouse blocked by low-level hooks; the pinned `platform.desktop.deskside` remains for bare-metal pinning. Desk pens and touch screens are not blocked (GAP-051) | Yes, by default (amended 2026-11-18, `privacy.desk`; before, `platform.desktop.deskside.enabled`, off): every display but the Pier's virtual ones dark through an all-zero transfer table, read back; an active HID event tap passes only the Pier's own events (`hosts/macos/src/desk_privacy.rs`, shared `console_session::desk_privacy`); handed back in `DETACH_STEPS` order; dies with the process, after which the agent's lock guard darkens the displays again and locks the session (`hosts/macos/src/lock_guard.rs`, shared `console_session::owner_death`; GAP-048 closed 2026-11-18) |
| No leftovers | Display journal and console recovery (`hosts/windows/src/console_recovery.rs:1-20`); window repatriation caveat (PD-032) | Virtual displays live in a child process and die with it (`hosts/macos/src/virtual_display.rs:58`, `:125`) |

The open differences against the principle are ledger gaps GAP-042
(Linux: a Wayland login screen, the resume hold across a lock, and a desk
with physical monitors), GAP-044 (Windows: a session held for a resume is
locked, so a returning Deck signs in again with its password), GAP-050 (Windows: the desk's monitors show the session until they
are detached) and GAP-051 (Windows: desk pens and touch screens are not
blocked). New Pier work adopts `console_session::DETACH_STEPS` and
`disables_while_attached`.

## Consequences

- A Linux workstation behaves like the other Piers: the Deck shows the
  user's own desktop with everything open, a remote-first sign-in is the
  same session the desk later unlocks, and leaving locks the session.
- The installer adds `pam_arcen_greeter.so`, `/etc/pam.d/gdm-ovirtcred`
  (built from the host's `gdm-password`) and a system-bus policy; it
  removes them on uninstall and never replaces another product's.
- `zbus` is linked in every Linux build (it already shipped in every
  release build through `wayland-provider`).
- An upgraded host with a display manager switches to joining the console;
  the administration guide says how to keep `dedicated`.
