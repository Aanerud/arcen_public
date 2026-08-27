# TLS Certificate Lifecycle

> **Status (2026-07-31): implemented for direct QUIC.** The shared policy and
> Windows/Linux Pier adapters serve QUIC on UDP 18444. The default
> `arcen-transport` build remains a
> dependency-light TLS lifecycle core; product crates opt into Quinn. Arcen
> Span remains dormant.

**Owners:** Shared/Architecture, Release/Security, Linux Host, Windows Host, and
macOS Client.

## Scope and modes

Piers accept one administrator-managed PEM certificate chain and one matching
private key. The configured paths remain `%ProgramData%\Arcen\tls\host.crt` and
`host.key` on Windows and `/root/arcen/tls/host.crt` and `host.key` in the
current Linux deployment. Material is validated before UDP bind and service
readiness. Pier does not fetch, issue, renew, rotate, or select certificates.

Two operational modes use the same runtime:

| Mode | Issuance and trust | Lifecycle |
| --- | --- | --- |
| Enterprise | An administrator supplies a CA-signed PEM chain and key. Deck uses its macOS system trust or an explicitly configured private CA bundle and validates the chain and server name. | Administrator replaces the files and explicitly reloads Pier. |
| SMB bootstrap | The packaging helper creates a local P-256 certificate only when both files are absent. Deck captures an otherwise-valid unknown-issuer leaf and asks for explicit, process-only SPKI trust. | The operator explicitly invokes same-key renewal or trust-changing rekey, then reloads Pier. |

There is no runtime `self_signed_auto` mode and no live Windows certificate
store mode. Generate-if-missing is installer/helper behavior, not Pier behavior.
Complete existing or custom files are never automatically overwritten; a
partial pair is an error.

## Shared rustls posture

`arcen-transport` uses rustls 0.23 with the ring provider and applies this
posture identically to both Piers:

| Control | Implemented rule |
| --- | --- |
| Versions | QUIC always negotiates TLS 1.3 as required by QUIC. Dormant WSS/TLS 1.2 support is available only in non-shipping `wss-compat` builds. |
| Suites | Shipped products expose only the three TLS 1.3 ring suites below, in provider order, minus the optional exact-name blacklist. An empty effective set is rejected. |
| Leaf key | RSA of at least 2048 bits, P-256, P-384, or Ed25519 only. |
| Leaf purpose | A CA leaf is rejected. If key usage exists, `digitalSignature` is required. If extended key usage exists, `serverAuth` is required and `anyExtendedKeyUsage` is rejected. |
| Identity | At least one syntactically valid DNS or IP SAN is mandatory. Every configured expected DNS/IP SAN must match exactly; CN fallback and wildcard matching for expected SAN policy are not used. |
| Time | Invalid intervals, not-yet-valid certificates, expired certificates, and unavailable trustworthy wall-clock time fail closed. The default expiry warning window is 30 days. |
| Key agreement | Exactly one admitted PKCS#8, PKCS#1, or SEC1 key must match the leaf certificate. |

The shipped suites, in effective preference order, are:

1. `TLS13_AES_256_GCM_SHA384`
2. `TLS13_AES_128_GCM_SHA256`
3. `TLS13_CHACHA20_POLY1305_SHA256`

Only an explicit non-shipping `wss-compat` build adds the six TLS 1.2
ECDHE-ECDSA/ECDHE-RSA AES-GCM and ChaCha20-Poly1305 suites.

The lifecycle work does not enable client authentication, key logging, or
early data. QUIC requires the private ALPN `arcen-quic-v1`. There is no
plaintext or WSS fallback.

## File and issuance boundaries

The Windows loader uses bounded opened handles, rejects reparse/device
traversal and unstable snapshots, and requires a private-key DACL restricted to
SYSTEM and Administrators. The Linux loader uses held directory descriptors,
`openat`, and `O_NOFOLLOW`; rejects traversal, symlinks, non-regular files,
unstable snapshots, untrusted owners, and oversized material; requires key mode
`0600` and certificate mode `0644` or stricter. Temporary key bytes are
zeroized.

The SMB helpers are separate privileged tools. They issue P-256/SHA-256,
`serverAuth`/`digitalSignature`, 825-day certificates with explicit hostname,
FQDN, and non-loopback IP SANs. They publish protected files transactionally.
Linux OpenSSL is a helper prerequisite only; the service neither links to nor
invokes it.

Same-key renewal preserves SPKI and therefore a new Deck session accepts the
renewed certificate after the operator has trusted that SPKI in the current
Deck process. Explicit rekey changes SPKI and prompts again. A legacy
whole-certificate bookmark remains a certificate pin and changes on every
certificate renewal; it is not silently upgraded to SPKI.

## Deck trust flow

QUIC uses the established Deck trust modes. High security keeps macOS
system/private-CA chain validation. Medium security first performs normal chain
and name validation. Only `UnknownIssuer` enters the capture path: Deck
validates the leaf's time, name, purpose, and signature requirements, extracts
the actual certificate SHA-256, SPKI SHA-256, and validity, and deliberately
rejects that probe handshake before showing them. No Arcen protocol field,
credential, or message is sent before trust.

The user can select only **Trust for Session** or **Cancel**. Acceptance stores
the SPKI in an endpoint-keyed map in the current Deck process; it is never
written to a bookmark or other persistent storage. A same-key renewal matches;
a rekey causes a new capture-and-reject prompt. Existing whole-certificate pins
retain their original semantics. The system/private-CA paths and the existing
Low-security double gate (`Security mode - Low` plus
`ARCEN_ACCEPT_INSECURE=1`) are unchanged.

## Reload and expiry

Replacement parsing and validation finish before the runtime resolver changes.
A valid replacement is atomically selected for new handshakes. A failed reload
retains the last good certificate while it remains valid. Windows uses SCM
control 202:

```powershell
sc.exe control ArcenPier 202
```

Linux `SIGHUP` independently attempts managed-log reopen/config reload and TLS
reload, so either can succeed when the other fails:

```sh
sudo systemctl reload arcen-pier
```

Established QUIC sessions retain their negotiated key and continue across
reload or later certificate expiry. The listener uses the reloadable resolver
for new handshakes. Once the active certificate expires, new QUIC handshakes
are refused.

## Rollout and upgrade

This is an intentional strict rollout break. Every SAN-less legacy certificate,
including output from the old Linux CN-only process, fails startup. There is no
compatibility bypass. Renew or replace material **before** upgrading or starting
the new service.

For helper-managed SMB material, preserve the key with:

```powershell
.\hosts\windows\scripts\new-host-cert.ps1 -Renew
sc.exe control ArcenPier 202
```

```sh
sudo /opt/arcen/bin/arcen-new-host-cert --renew --directory /root/arcen/tls
sudo systemctl reload arcen-pier
```

Use `-ForceNewKey` on Windows or `--new-key` on Linux only for an intentional
trust change. Enterprise administrators should obtain a SAN-bearing
server-auth chain, stage the complete chain and matching key in the protected
TLS directory, apply the required ACL/modes, atomically replace the configured
files, invoke the platform reload command above, and confirm the TLS activation
event. On a stopped host, replace both files before starting Pier. Failed
validation leaves a running host on its last good still-valid certificate, but
a stopped host will not start.

## Privacy and support

Lifecycle logs/events may contain only bounded safe metadata such as
certificate/SPKI hashes, admitted key class/size, validity/expiry state, SAN
counts, warning days, source/component labels, and closed reason classes. They
exclude PEM, private-key metadata, SAN values, subject/issuer names, paths, and
raw validation errors.

Support Bundle does not stat, open, hash, copy, or serialize either configured
certificate/key path or either file. It may include the already-sanitized,
bounded lifecycle metadata present in approved logs/events.

## Deferred

- Windows CNG/certificate-store and non-exportable signing
- runtime `self_signed_auto` or any implicit issuance/rotation
- ACME, SCEP, and ADCS enrollment
- persistent Deck trust bookmarks
- mTLS and protocol-advertised pins/messages
- multi-stream/datagram QUIC optimization and Arcen Span
- OIDC and licensing integration
- 0-RTT and runtime certificate fetch

## Validation and limitations

Shared policy, host adapters, helper transaction behavior, Deck trust
decisions, and reload state have automated unit/portable coverage. This
worktree does **not** establish native acceptance for the macOS UI/system trust
path; Linux `openat`, systemd, journal, or installed-helper behavior; Windows
installed DACL, SCM control, Event Log, or service reload; real concurrent QUIC
renewal; old-binary upgrade behavior; or cross-host lab operation. Those
platform/lab checks remain required and must not be reported as passed.

The implementation and this document are original Arcen work; no source intake
or provenance row accompanies this milestone.
