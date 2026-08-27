# Pre-session disclaimer banner

The direct-connection Windows and Linux Piers can require an operator-provided
disclaimer before Deck collects or sends credentials. This is an additive
protocol-v3 feature: it adds no message type, FSM state, session-grant claim, or
authorization decision.

## Contract and ordering

1. At startup, the host selects the operator-configured locale (default
   `en_US`), reads `<locale>.txt` once, and prepares exact content through
   `arcen-identity`.
2. Preparation requires a safe, bounded ASCII locale and non-empty valid UTF-8
   content no larger than 16 KiB. Content is rejected, never truncated or
   normalized.
3. The host sends the exact text as optional `AuthRequest.disclaimer`.
4. Deck displays the exact selectable text and host identity before its
   credential screen. Accept advances to credentials. Decline closes the same
   socket, clears secret/state, disables automatic reconnect, and sends no
   `AuthResponse`.
5. One `AuthResponse` carries optional
   `disclaimer_acceptance_sha256`, exactly 64 lowercase hexadecimal characters
   over the displayed UTF-8 bytes.
6. An enabled host rejects absent, invalid, or mismatched acknowledgment before
   PAM validation, PAM/launcher semaphore work, `LogonUserW`, Credential
   Provider handoff, or session launch. Close and the bounded ten-minute
   interaction timeout fail at the same boundary.
7. After successful OS authentication, the host records standalone
   `DisclaimerAcceptance` evidence: locale, digest, host epoch time,
   correlation/session identity, and success.

No event contains banner text, credentials, username/domain/SID/PAM account,
peer data, secrets, or raw errors. The existing lifecycle IDs and support-bundle
privacy policy are reused; there is no disclaimer-specific stable lifecycle ID.

## Shared and component boundaries

- `arcen-identity` owns `DisclaimerLocale`, `PreparedDisclaimer`, strict digest
  parsing/matching, and standalone `DisclaimerAcceptance`. It performs no file
  I/O, clock access, authentication, or logging.
- `arcen-protocol` owns the optional/defaulted/omitted-when-absent
  `AuthRequest.disclaimer` and
  `AuthResponse.disclaimer_acceptance_sha256` fields. Protocol remains v3.
- Deck owns interactive display and decision state. Its smoke connector returns
  `DisclaimerRequired` rather than accepting.
- Windows Pier owns strict nested `auth.disclaimer` config, startup preparation,
  and the broker gate. Prepared text never enters agent configuration or IPC.
- Linux Pier owns `--disclaimer`, `--disclaimer-dir`, and
  `--disclaimer-locale`, startup preparation, and the PAM/launcher gate.

`SessionGrantClaims` v1 is unchanged. Acceptance is not cryptographically bound
to a signed grant and must not be described as grant authorization. A signed
schema-v2 design is deferred.

For the separate direct-QUIC resume grant, the accepted disclaimer's
digest/version is fixed when the initial session opts in. A detached resume
request omits banner text and revalidates that existing binding; it does not
create new acceptance evidence. See
[`session-auto-reconnect.md`](session-auto-reconnect.md).

## Peer compatibility

| Host | Deck | Result |
| --- | --- | --- |
| Old host | New Deck | No fields are present; existing auth/no-auth flow is unchanged. |
| New host, feature off | Old or new Deck | Optional fields are omitted; serialized protocol-v3 behavior is unchanged. |
| New host, feature on | Old/unacknowledging Deck | Host rejects before OS authentication because acknowledgment is absent. |
| New host, feature on | New Deck, Accept | Deck sends the exact digest with credentials; matching acknowledgment permits existing OS auth. |
| New host, feature on | New Deck, Decline/close/timeout | No auth response or OS-auth side effect; connection closes and does not auto-reconnect. |

## Operator configuration

Windows `pier.json`:

```json
{
  "auth": {
    "disclaimer": {
      "enabled": true,
      "locale": "en_US",
      "directory": "disclaimers"
    }
  }
}
```

The default directory is `%ProgramData%\Arcen\disclaimers`; relative paths
resolve from `pier.json`. Both normal startup and `validate-config` load and
validate the file before listening or service work.

Linux:

```text
arcen-pier --auth-mode pam --disclaimer \
  --disclaimer-dir /etc/arcen/disclaimers \
  --disclaimer-locale en_US
```

Disclaimer mode is incompatible with no-auth. Preparation completes before
`net::serve`.

The feature, operator wording, and authentication boundary require
Shared/Architecture, both host owners, Deck ownership, and
Release/Security/legal review. It makes no certification, compliance, or
legal-sufficiency claim.
