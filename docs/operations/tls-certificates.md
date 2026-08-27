# Operating TLS Certificates

This runbook covers direct QUIC on UDP 18444. The listener uses the
operator-managed PEM and reload lifecycle. It does not
apply to Arcen Span. Pier never issues, renews, rotates, or fetches
certificates.

See
[`../architecture/tls-certificate-lifecycle.md`](../architecture/tls-certificate-lifecycle.md)
for the complete posture and trust model.

## Before install or upgrade

The leaf must be currently valid, must not be a CA, and must contain at least
one valid DNS or IP SAN. If key usage is present it must include
`digitalSignature`; if extended key usage is present it must include
`serverAuth`. The key must match and be RSA 2048-bit or stronger, P-256, P-384,
or Ed25519. Every configured expected SAN is an additional exact requirement.

**Rollout break:** SAN-less legacy certificates, including the old Linux
CN-only output, prevent Pier from starting. There is no compatibility bypass.
Renew or replace them before the service upgrade/start. Do not depend on CN
fallback.

## Enterprise CA-issued PEM

Obtain a complete leaf/intermediate chain and matching key from the enterprise
CA. Provision them at the existing configured locations:

- Windows:
  `%ProgramData%\Arcen\tls\host.crt` and
  `%ProgramData%\Arcen\tls\host.key`
- Current Linux deployment:
  `/etc/arcen/host.crt` and `/etc/arcen/host.key`

On Windows, retain only SYSTEM and Administrators in the key DACL. On Linux,
use root ownership, key mode `0600`, and certificate mode `0644` or stricter.
Reject symlinks/reparse points. Stage and validate the complete chain and key
inside the protected directory before atomically replacing the active files.
On a stopped host, replace both before starting Pier.

For a running host, replace both files and request validation/reload:

```powershell
sc.exe control ArcenPier 202
```

```sh
sudo systemctl reload arcen-pier
```

Previous-helper pairs without an ownership marker require one explicit
same-key adoption. Use `-Renew -AdoptLegacyHelperPair` on Windows or
`--renew --adopt-legacy` on Linux. These switches are only for known Arcen
helper output; never adopt enterprise/custom PEM.

Confirm the TLS activation event before removing the staged backup. A failed
reload retains the last good certificate while valid. At expiry, Pier refuses
new handshakes. Deck continues to use macOS system trust or the configured
private CA chain; no TOFU prompt is expected for a trusted, name-valid chain.

## SMB bootstrap and renewal

The helpers generate P-256/SHA-256, server-auth certificates with explicit
hostname, FQDN, and non-loopback IP SANs. With no issuance flag, they generate
only when both files are absent. A complete current/custom pair is retained;
a partial pair fails. OpenSSL and util-linux `flock` are required only by the
Linux helper. Both helpers serialize issuance and bind their ownership marker
to the current certificate/key pair; explicit renewal or rekey refuses custom
material or a stale marker.

Windows initial generation, in elevated PowerShell 7+ from the repository:

```powershell
.\hosts\windows\scripts\new-host-cert.ps1
```

Linux deployment installs the helper and invokes generate-if-missing for the
current path:

```sh
sudo /opt/arcen/bin/arcen-new-host-cert --directory /etc/arcen
```

### Same-key renewal

Use this before upgrade when replacing a legacy helper certificate, and for
ordinary explicit renewal:

```powershell
.\hosts\windows\scripts\new-host-cert.ps1 -Renew
sc.exe control ArcenPier 202
```

```sh
sudo /opt/arcen/bin/arcen-new-host-cert --renew --directory /etc/arcen
sudo systemctl reload arcen-pier
```

Same-key renewal changes the whole-certificate hash but preserves SPKI. New
Deck process trust accepts the renewed leaf once that SPKI is explicitly
trusted for the process. Legacy whole-certificate pins remain certificate pins
and therefore change.

### Explicit trust-changing rekey

```powershell
.\hosts\windows\scripts\new-host-cert.ps1 -ForceNewKey
sc.exe control ArcenPier 202
```

```sh
sudo /opt/arcen/bin/arcen-new-host-cert --new-key --directory /etc/arcen
sudo systemctl reload arcen-pier
```

Rekey changes SPKI. Verify the new hash out of band; Medium-security Deck
connections capture-and-reject the unknown-issuer probe and prompt again.

## Reload behavior

Windows SCM control 202 reloads TLS only. Linux `SIGHUP` independently attempts
both logging and TLS reload, allowing one to succeed when the other fails.
Only a completely validated replacement is swapped into the runtime resolver.
Established QUIC sessions keep the already negotiated key. No plaintext or WSS
fallback is opened.

## Diagnostics and acceptance limits

Use bounded TLS lifecycle logs/events to confirm activation, warning, reload,
or closed reason classes. Support Bundle never inspects certificate/key files
or paths; see [Support Bundles](support-bundles.md).

Automated coverage does not replace native acceptance. Native macOS UI/system
trust; Linux `openat`, systemd, journal, and installed-helper behavior; Windows
installed DACL, SCM, Event Log, and service reload; real concurrent QUIC
renewal; old-binary upgrade; and cross-host lab validation were unavailable in
this worktree and must not be claimed as passed.
