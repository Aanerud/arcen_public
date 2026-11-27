# Windows console policy

Arcen Pier for Windows serves the physical console session. The Windows Credential Provider signs
the requested account in at the console, and desktop capture reads the console display. Pier does
not use RDP or RDS, so it cannot capture an arbitrary per-user remote session.

Windows client SKUs can have several signed-in users through Fast User Switching, but only one WTS
session is attached to the physical console at a time. Without RDS, Arcen cannot serve two users
simultaneously from one Windows host. A second Arcen user must wait until the console can safely be
reassigned.

## Locked is not disconnected

Locking the screen does not free the console. In WTS terms a locked console session is still
`Active`: it still owns the physical console and may have a user standing at the machine. Arcen
treats that as occupied.

A `Disconnected` session is different. Windows reports that no client is attached to that session.
A disconnected session may still be signed in, but it is not actively driving the console. Arcen's
takeover rule applies only to this state.

## Current decisions

| Observed console state | Arcen decision | Client message on refusal |
| --- | --- | --- |
| Requested account owns the active console, unlocked | Bind to that session and start capture | — |
| Requested account owns the active console, **locked** | Unlock it through the Credential Provider | Credential Provider failures are reported separately |
| Nobody owns the console, nothing else competes for it | Sign the account in at the console | Credential Provider failures are reported separately |
| Requested account has exactly one disconnected session elsewhere, console otherwise free | Move that session onto the console, then bind | `…: the account's existing session could not be moved to the physical console.` |
| **Another account owns the console, WTS reports `Active`** | **Refuse.** A locked console counts as active. No sign-out is offered yet (see below) | `console_other_user: another account is signed in at this workstation.` |
| **Another account's session is attached through another remote-desktop service** (an RDP or ICA client is connected to it) | **Refuse** | `console_other_user_other_product: another remote-desktop service (RDP-Tcp, rdp) uses this workstation.` — the session's WinStation listener and client protocol, as Windows reports them |
| The requested account's own desktop is attached through another remote-desktop service | Refuse; one remote product at a time | `console_in_use_other_product: this desktop is in use through another remote-desktop product.` |
| **Another account owns the console, WTS reports `Disconnected`, and this account has a parked session** | **Take over**: move this account's session onto the console | — unless a later Windows operation fails |
| Another account owns the console, WTS reports `Disconnected`, and this account has nothing to move | Refuse | `…: another account holds the console and this account has no session to move onto it.` |
| The console owner cannot be verified | Refuse; Arcen does not guess ownership | `…: active console account could not be verified.` |
| Console is ambiguous, non-console, stale, or has unverifiable interactive sessions | Refuse | The specific classifier reason follows `Remote sign-in is unavailable:` |
| The requested account's session is held for another Deck's resume, and that Deck's connection is gone | **Take over** at once: the held session is handed to this sign-in, unlocked (the resume grant is invalidated) | — |
| Another account's session is held for its Deck's resume | Refuse | `Another user's remote desktop is waiting for its owner to reconnect; try again later.` |
| The requested account appears to own more than one possible desktop | Refuse; choosing one would be a guess | `…: authenticated account session is ambiguous.` |

A refusal Arcen can name leads with its code (the shared console refusals, the
same codes a Linux Pier uses; see [error codes](../operations/error-codes.md)).
Every other client message is prefixed `Remote sign-in is unavailable:` and
abbreviated above with `…` for width. The Pier log records the code as
`reason_class`, and for another remote-desktop service its `win_station` and
`protocol`.

Client-facing messages name the situation, not the other account. Operators need to know that
another account owns or is using the console; naming the account would expose avoidable user
identity information to a remote client and is not needed to resolve the policy decision.

## Another account's idle desktop: no sign-out offered yet

ADR 0025 §13 and `arcen_session::session_handover` decide; the Windows Pier
supplies the facts. Who holds the desktop is the account's SID, the WTS
session id, its logon id (new for every sign-in, so a reused session id is
another session) and the boot. A client is attached when an Arcen Deck's
agent serves that account, or when Windows reports the session `Active` or
`Connected` off the console (another remote-desktop service). That
service is using the session, so the account's own sign-in is refused
`console_in_use_other_product`, and the session is never moved off its
client (`RemoteClient::OwningProduct`), as before 0.21.5. Every Arcen
attachment moves the desktop's epoch on, and none attaches to a session a
confirmed change of account is signing out.

The Pier has the whole confirmed change of account: the offer after the
password, Cancel as the default, the facts read again before anything
happens, the ordinary sign-out asked from inside the target session
(`arcen-pier sign-out-session`, `ExitWindowsEx(EWX_LOGOFF)` without a
force flag) and the exact WTS session and logon read back gone before the
requester signs in through the Credential Provider. It does not offer it
(`SwitchRoute::PierCannotSignOut`), so another account is refused as before:
Windows carries that sign-out out only for an unlocked session. A locked
session, which is every session a Deck has left, or a disconnected one
stays signed in and Windows drops the request without an error (Windows 11
Pro build 26300, measured 2026-11-26: the same request signed an unlocked
session out in three seconds). The routes that would work on a locked
session force the sign-out and lose unsaved work (`EWX_FORCE`,
`WTSLogoffSession`); Arcen never uses them. Unlocking the session first
would need its owner's password and would show their desktop.

The Pier also never claims that nothing is attached to a session: a
product that serves the console session itself, rather than a session of
its own (an agent that captures the console, as remote-access servers do),
leaves no trace in WTS, and the Pier does not yet prove those routes (for
example by the known remote-access server processes and their established
connections). Such a session counts as unknown: its own account joins it,
keeping its layout, and another account is refused.

## When the Deck leaves

Arcen joins the user's session and never ends it (ADR 0025). When a Deck
leaves for good (it disconnects, it did not ask to resume, its reconnect window
ends, a resume is refused, or the Pier stops), the session agent restores the
displays and gives the desk its input back, then the Pier locks the session
(`LockWorkStation`, run as the user in that session) and waits until Windows
reports it locked; the desk's speakers come back last. A session that will not
lock is disconnected from the console instead, so the desk shows the sign-in
screen and the account's next sign-in reconnects the same session. Windows
refuses display changes from a locked session, which is why the displays are
restored before the lock (`console_session::DETACH_STEPS_OUTPUTS_FIRST`).

With `privacy.desk = "required"`, and for a session that is already locked when
its Deck leaves, the lock comes first (`console_session::hand_back_order`): the
session agent stays, keeping the desk dark with its keyboard and mouse blocked,
until the session is locked or disconnected; then it exits, and the displays it
can no longer restore from the locked session are restored behind the lock
screen by `restore-display --console`, which runs as LocalSystem on the
console's input desktop. A session that refuses both is asked again every 5
seconds for a minute and then every 30 seconds, and its desk stays dark and
blocked meanwhile: `required` never gives the desk back unlocked. The session
agent applies the same rule however its attachment ends (a desk check that
refused the desk, an error, the broker gone): it locks its own session and
confirms the lock before it gives the desk anything back. Ctrl+Alt+Delete still
reaches Windows, so a person at the dark desk can choose Lock there and sign in
again. The refusal
is logged at error level, and a Deck that signs in meanwhile is told why the PC
is busy. A Pier that stops, or a Windows that shuts down, does not wait for it.

The session is not locked when a sign-in of the same account takes it over
(including one that waits while the departing Deck's session is handed back),
or when it is no longer the session the Deck joined (signed out, locked or
taken over at the desk). A sign-in refused after it unlocked or signed the
session in, or took it over, leaves it locked. If the departing broker locked
it just before a same-account sign-in took it over, that sign-in is refused
with `console_session_changed`, and signing in again unlocks it.

A Deck whose connection drops is held for its resume (`auth.reconnect_window_secs`),
and its session is locked at once, while the session agent keeps the desk as
private as the attachment had it. Windows cannot unlock a session without the
user's password, and a resume carries none, so a returning Deck that finds its
session locked is asked to sign in again (resume code `unsupported`, reason
"The session was locked when the connection dropped. Sign in again to unlock
it."). The hold continues for that sign-in: it unlocks the same session through
the credential provider and takes the held desktop over, with its applications
and windows as they were. The user therefore sees a password prompt when a Deck
comes back after a lost connection. A session unlocked at the desk meanwhile is
resumed without one. A returning Deck whose session changed meanwhile (signed
out, another account) is refused with `console_session_changed`. Putting the
held session back without a password, by taking it off the console while held,
is tracked as GAP-044.

## Takeover configuration

No additional setting is required. The policy is deliberately narrow instead of
administrator-configurable: Arcen only takes over when Windows itself reports the other console
session as `Disconnected`. It never displaces an `Active` session, including a locked one.

Two limits are worth stating plainly, because they surprise people:

**Takeover needs something to move.** Arcen reaches an inactive console by moving a desktop the
signing-in account *already owns* onto it (`WTSConnectSessionW`). If that account has no parked
session, there is nothing to move and Arcen refuses. It will not ask Windows to sign the account
into the other account's session id: that is not something Windows can do, and attempting it fires
a Ctrl-Alt-Del at the occupied console before failing.

**The check is re-run at the last moment.** A classification is a snapshot, and a person can sit
down at the machine between the decision and the action. Arcen re-reads the target session's state
immediately before moving it and aborts if it has become `Active`.

## Scope

This local Pier policy is deliberately narrow: it only handles the inactive/disconnected console
case needed to make a single-console Windows host reachable without RDS. It never offers an
administrator "kick out" of an active console user.
