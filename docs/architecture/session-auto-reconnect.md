# Direct-QUIC Session Auto-Reconnect

**Status (2026-07-21):** implemented in the current draft branch for macOS Deck
and the Windows and Linux Piers; pending Shared/Architecture,
Release/Security, macOS Client, Windows Host, and Linux Host review, plus
target-OS interruption/crash validation.

## Scope

This feature is only macOS Arcen Deck connecting directly to a Windows or Linux
Arcen Pier over QUIC/TLS 1.3 on UDP 18444. It adds no Arcen Span/Gateway,
network fetch, persistence, cross-host state, OIDC, entitlement, licensing,
mTLS, or 0-RTT behavior, and does not bump protocol version 3.

The owning surfaces are:

- `arcen-protocol`: additive authentication fields and stable failure codes;
- `arcen-identity`: strict direct-resume claims, canonical signing bytes, and
  signer/verifier contracts;
- `arcen-session`: the pure reconnect policy/state machine and current
  generation+nonce slot;
- `arcen-transport`: a direct-QUIC replacement binding over connection, stable
  host, and active-session identity;
- Deck and each Pier: sockets, clocks, process-local key ownership, native
  identity checks, resource lifecycle, and UI/media behavior.

The exact wire contract is in
[`../../shared/protocol/WIRE.md`](../../shared/protocol/WIRE.md). The resume
authority decision is [ADR 0005](../adr/0005-direct-transport-resume-authority.md).

## Authority and bindings

Each Pier process creates a fresh system-random 256-bit key and keeps it only in
zeroizing process memory. Host adapters use ring HMAC-SHA-256 over
`arcen-identity`'s length-delimited canonical bytes with the exact domain
`arcen-direct-quic-resume-v1`. The stable host identity is
`spki-sha256:<validated TLS SPKI hash>`, not an address.

Claims bind the active host session, native SID+WTS session or uid+logind
session, Deck holder nonce, current generation and host nonce, disclaimer
digest/version, issue and expiry times, and stable host identity. The host also
retains a hash of client dimensions, monitor layout, display mode, and time
zone. A mutex-protected one-slot compare-and-rotate permits only the exact
current generation+nonce; one concurrent attempt wins.

Each claim lifetime is `2W`, where the configured post-loss window `W` defaults
to 1200 seconds and must be in `0..=7200`; the maximum accepted claim lifetime
is therefore 14,400 seconds. An opted-in attached session rotates its one slot
and sends a successor every `W/2`, using millisecond precision for odd and
one-second windows. Deck stores the grant/window while connected but no
deadline. At unexpected transport loss, Deck and host independently anchor
their exact monotonic, generation-tagged deadline at `loss_now + W`;
`now >= deadline` is expired and failed resume retries preserve that deadline.
Zero advertises and issues nothing and gives the auto-reconnect state machine no
detach grace period: attachment/media cleanup and reconnect-held resource
restoration begin immediately. On Linux, the pre-existing PAM-authenticated
persistent-desktop cache remains a separate lifecycle and still requires full
authentication; zero does not turn that cache into credential-free resume.

Both Piers enforce one active product session: Windows holds one
`BrokerAgentLease`; Linux holds one session semaphore and one
`SessionRegistry` desktop. Each resume authority likewise contains one current
slot, not a token map.

## Initial connection

1. An opt-in Deck creates one memory-only holder nonce and sends
   `resume_requested=true` only if `AuthRequest.auth_methods` advertises
   `resume`.
2. Normal disclaimer and PAM/password/Credential Provider processing runs.
   Resume never replaces initial native authentication.
3. The Pier fixes the TLS SPKI, native principal/session, active session,
   disclaimer evidence, topology/time-zone choice, and held display/time-zone
   ownership. Windows also waits for its per-session agent; Linux waits for the
   dedicated-Xorg launcher/agent and active same-uid logind session.
4. The host installs generation 1 and returns its opaque grant and window in a
   successful `AuthResult`. Deck stores them only in its reconnect controller
   without starting a deadline and retains no password in reconnect options.

Old peers do not opt in: an old host never advertises `resume`, and an old Deck
does not send the new request fields. Their existing protocol-v3 path is
unchanged.

## Attached refresh ordering

Only an opted-in resumable attachment owns one refresh interval; legacy and
non-opted-in attachments receive no refresh. Every `W/2` while `Attached`:

1. Under the registry mutex, the host reads the current generation+nonce,
   creates a random successor, compare-and-rotates exactly once, signs a `2W`
   claim, and leaves the registry `Attached`.
2. Before further media, the same ordered QUIC stream sends protocol-v3
   `type="auth_result"`, `success=true`, a bounded message, the successor grant
   and window, `resumed=false`, and no `error_code`.
3. Deck treats that exact shape as a typed streaming refresh, atomically
   zeroizes/replaces the old grant, records the current Session Log ID for
   diagnostics, and continues without `AcceptAuthentication`.

A failure or malformed mid-stream `AuthResult` is terminal. Rotation precedes
send intentionally. A known successor serialization/send failure immediately
revokes the slot and enters final drain. If delivery loss is learned only when
Deck later presents its cryptographically valid, unexpired exact predecessor
while the slot is detached,
the host authenticates all immutable bindings, recognizes only
`candidate_generation + 1 == current_generation`, and enters the same final
drain while returning `replayed`. Neither path reopens the predecessor or
reveals the successor. Normal visible authentication can proceed after cleanup
removes the entry and releases the one-session lease.

## Detach ordering

Only unexpected EOF, reset, liveness timeout, or selected transient transport
I/O enters auto-reconnect. Explicit WebSocket Close, manual disconnect,
authentication/protocol/TLS-identity/configuration failures, and native-session
termination are terminal.

1. The shared transition records one host monotonic deadline at loss and
   Deck records its own deadline from that same loss observation. The transition directs the owner
   to hold restore leases, stop media, reset input, close the failed transport,
   and arm one generation-tagged timer.
2. Windows marks the one slot detached immediately, before requesting private
   agent attachment cleanup. A verified resume handoff may therefore queue in
   the owner channel while cleanup finishes.
3. Windows performs attachment cleanup: close/reset input, capture/encode,
   audio, writer, and attachment queues. It retains broker-agent IPC and
   process, `BrokerAgentLease`, display lease and recovery watchdog/journal,
   time-zone lease, and native-session monitor.
4. Linux completes attachment media/input/audio cleanup, then transfers its
   `HeldDisplayResources` (`MetaModeGuard` plus one-session permit) into
   `SessionRegistry`. The launcher, user agent, dedicated Xorg, desktop/apps,
   display mode, and process-tree time-zone lease remain alive.
5. Linux marks its slot detached only after that cleanup/transfer and then
   admits resume handshakes.
6. Deck drops the old transport and media worker, releases remote keys, keeps
   the last uploaded texture as a dimmed frozen frame, and retries from
   memory-only state with randomized exponential backoff capped at five seconds
   and by the exact loss-anchored deadline. Failed resume transports do not
   restart it.

## Resume ordering

1. Deck establishes a fresh QUIC/TLS connection and creates a fresh Session Log
   ID. While a slot is detached, the Pier advertises `resume` without sending
   the already-bound disclaimer.
2. Deck sends `method="resume"` with empty username and credential, the holder
   nonce, opaque current grant, exact topology/time zone, and new diagnostic ID.
3. Under the registry mutex, the Pier validates shape, HMAC, time, TLS SPKI,
   active session, native principal, holder, disclaimer, topology, and the
   current slot. It then compare-and-rotates and signs the successor before
   handing the socket to the existing owner. No failure falls back to native
   authentication.
4. The owner re-observes native state. Windows checks the same unlocked
   SID/WTS session and asks the held agent to validate its exact output. Linux
   checks launcher/agent health and active same-uid logind ownership, then takes
   the same held display resources from `SessionRegistry`.
5. Before media restarts, the host returns `success=true`, `resumed=true`, the
   successor grant, and the window. A response serialization/send failure
   immediately revokes resume and follows final cleanup; Linux retains the
   taken display resource for that cleanup rather than re-holding it for
   another detach. The lost successor is never reconstructed or disclosed.
6. Windows sends the agent a fresh attach command. Linux creates a fresh
   attachment on the same display. Both create fresh capture/audio/queue state
   and force/request a fresh IDR without reapplying display or time-zone state.
   Deck accepts the successor first, creates a fresh inbox, decoder, and audio
   worker, requests a full frame, and does not replace the frozen frame until a
   decodable fresh frame arrives.

Successful resume replaces the grant/window and clears the completed detach
deadline. The new attached refresh interval is owned by the new attachment;
detached, resuming, and draining states never refresh.

Every successful cycle rotates the token. Concurrent or duplicated use of the
old token is rejected as `replayed`. On a detached slot, an authenticated exact predecessor is the
only stale generation that can trigger terminal owner drain; older, ahead,
malformed, wrongly signed, expired, or incorrectly bound candidates leave a
detached slot unchanged. Concurrent predecessor replay has one terminal winner.

## Terminal drain ordering

Explicit close, expiry, native identity/session loss, terminal TLS/topology
change, agent/launcher death, Pier shutdown, known post-rotation successor
delivery failure, or an authenticated exact predecessor on a detached slot enters one idempotent
drain. Stale timers and repeated terminal events are no-ops.

1. Revoke the current resume slot first, cancel its timer, and prevent another
   handoff.
2. Stop/reset any attachment resources and close attempted transports.
3. Windows closes and bounded-waits the agent. The agent's final cleanup
   restores/disarms display ownership; its watchdog and journal remain active
   until that boundary. Only after agent exit does the broker restore the
   system-wide time zone and release `BrokerAgentLease`. Failed restores retain
   their existing journals for watchdog/recovery.
4. Linux takes any transferred `HeldDisplayResources`, restores/releases them
   once, then terminates the launcher, user agent, and dedicated Xorg. Confirmed
   launcher shutdown completes the in-memory process-tree time-zone lease.
5. Remove/complete the registry entry only after cleanup. Process restart or
   crash loses the HMAC key and slot and is never resumable; existing platform
   watchdog/PDEATHSIG and restore boundaries own recovery.

## Diagnostics, compatibility, and non-claims

Each initial or resume connection has a new Deck-generated Session Log ID.
Hosts log the bounded `previous_sid` to chain attempts; these IDs are
diagnostic only and never authorize resume. Grants, holder nonces, passwords,
native principals, and HMAC keys are redacted and must not enter logs or support
bundles.

The initial disclaimer acceptance is fixed into the grant as digest/version.
Resume does not display or accept a new disclaimer and cannot change that
evidence. See [disclaimer-banner.md](disclaimer-banner.md). Display and
time-zone leases remain held rather than reapplied; see
[timezone-redirection.md](timezone-redirection.md). TLS/SPKI behavior remains
the direct-QUIC lifecycle in [transport.md](transport.md) and
[tls-certificate-lifecycle.md](tls-certificate-lifecycle.md).

Unit/adversarial coverage exercises additive legacy JSON, strict in-band
refresh parsing, refresh redaction and replacement, long attached sessions with
no pre-loss deadline, exact retry deadlines, exact `2W` claim bounds, token
tamper and binding mismatches, one concurrent rotation winner, exact-predecessor
drain, older-token rejection, successor-send fault injection, stale timers,
native/topology terminal behavior, and one-time crash/shutdown cleanup. This is
not a claim of external audit, certification, production soak, or completed
Windows/Linux/macOS lab validation.
