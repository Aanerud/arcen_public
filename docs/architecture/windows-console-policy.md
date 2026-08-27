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
| **Another account owns the console, WTS reports `Active`** | **Refuse.** A locked console counts as active | `…: another account is actively using the physical console.` |
| **Another account owns the console, WTS reports `Disconnected`, and this account has a parked session** | **Take over**: move this account's session onto the console | — unless a later Windows operation fails |
| Another account owns the console, WTS reports `Disconnected`, and this account has nothing to move | Refuse | `…: another account holds the console and this account has no session to move onto it.` |
| The console owner cannot be verified | Refuse; Arcen does not guess ownership | `…: active console account could not be verified.` |
| Console is ambiguous, non-console, stale, or has unverifiable interactive sessions | Refuse | The specific classifier reason follows `Remote sign-in is unavailable:` |
| The requested account appears to own more than one possible desktop | Refuse; choosing one would be a guess | `…: authenticated account session is ambiguous.` |

Every client message is prefixed `Remote sign-in is unavailable:` and abbreviated
above with `…` for width.

Client-facing messages name the situation, not the other account. Operators need to know that
another account owns or is using the console; naming the account would expose avoidable user
identity information to a remote client and is not needed to resolve the policy decision.

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

## Future gateway behavior

A future Arcen Gateway may provide an explicit administrator "kick out" workflow with audit and
operator confirmation. This local Pier policy is narrower: it only handles the
inactive/disconnected console case needed to make a single-console Windows host reachable without
RDS.
