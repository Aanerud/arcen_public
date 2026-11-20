# Operating TLS Certificates

This runbook covers direct QUIC on UDP 18444. The listener uses the
operator-managed PEM and reload lifecycle. Pier never issues, renews, rotates, or fetches
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
- macOS Pier (preview):
  `/Library/Application Support/Arcen/tls/host.crt` and
  `/Library/Application Support/Arcen/tls/host.key`

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

```sh
# macOS Pier (preview)
sudo launchctl kill HUP system/pier.arcen.tech.service
```

An ordinary install or upgrade takes over a self-signed pair without an
ownership marker only when it positively matches the legacy Arcen installer
profile. The key is kept, paired Decks keep trusting the host, and the
certificate is reissued with the marker while preserving the previous DNS/IP
SANs and merging any new install-time names. Operator self-signed material,
CA-issued pairs, and files with an invalid or mismatched marker are served as
operator-managed material and never reissued implicitly. Only the renewal
helpers still need to be told:
`-Renew -AdoptLegacyHelperPair` on Windows or `--renew --adopt-legacy` on
Linux, and never for enterprise/custom PEM.

Residual adoption risk: a marker-less operator-made self-signed certificate
that exactly reproduces one of Arcen's historical self-signed profiles in the
Arcen TLS directory, including any matching companion pin files that profile
wrote, is indistinguishable from an old Arcen install and will be adopted. To
opt out, install a marker-less CA-issued certificate or a self-signed
certificate with a deliberately different profile.

Confirm the TLS activation event before removing the staged backup. A failed
reload retains the last good certificate while valid. At expiry, Pier refuses
new handshakes. Deck continues to use macOS system trust or the configured
private CA chain; no TOFU prompt is expected for a trusted, name-valid chain.

## SMB bootstrap and renewal

There is one implementation per platform, and every decision in it is
shared. The Linux and macOS Piers' `new-host-cert` and the Linux and Windows
installers issue one profile: P-256/SHA-256, subject `CN=Arcen Pier`, critical
`CA:FALSE` and `digitalSignature`, `serverAuth`, explicit hostname, FQDN and
non-loopback IP SANs, valid for 825 days from five minutes before issue. Each
checks the certificate against that profile, and against the key it was made
over, before installing it. A renewal or adoption keeps the existing key file
unchanged, whether it is SEC1 (`openssl ecparam`, what earlier Linux tools
wrote) or PKCS #8, and keeps every DNS/IP name the old certificate covered,
adding any new ones; only an explicit rekey changes the names or the key. New
keys are PKCS #8. The helpers are front ends: `arcen-new-host-cert`
(`packaging/linux/new-host-cert.sh`) runs `arcen-pier new-host-cert`, and
`new-host-cert.ps1` runs `install-arcen-pier.exe --tls-only`. Neither needs
OpenSSL.

With no issuance flag, material is created only when both files are absent,
kept while current, and renewed over its key when due. The Linux and Windows
installers additionally take over an unmarked self-signed pair only after
shared legacy-Arcen classification succeeds, by reissuing over its existing
key and writing the shared ownership marker; operator self-signed and
CA-issued pairs are retained. A partial pair fails. Explicit renewal or rekey
refuses custom material or a stale marker.

Every run replaces the files by the same journalled transaction: stage and
check every replacement, write the journal, back up what is replaced, publish,
check the published pair, commit, clean up. A run that dies at any step is
finished or undone by the next run of any of them, before it decides anything,
so the directory always holds the whole old pair or the whole new one. Each
holds the directory's exclusive lock (`.arcen-cert.lock`) for the whole
transaction, so a second run while one is active is refused rather than
interleaved. The Linux and macOS tools refuse a certificate directory that is
writable by anyone but its owner (and on Linux one reached through a link).

Windows initial generation, in elevated PowerShell 7+ from the repository
(`-Installer` names `install-arcen-pier.exe`, for example a repository build,
when it is neither beside the script nor in Program Files):

```powershell
.\hosts\windows\scripts\new-host-cert.ps1
```

or directly:

```powershell
install-arcen-pier.exe --tls-only
```

`--tls-only` restricts the `tls` directory it is given; the folder above it
keeps its own permissions unless it is the default `%ProgramData%\Arcen`.
The installer refuses a `tls` or Arcen folder that is a junction or link
anywhere in its path, or that is owned by an account other than SYSTEM or
Administrators; delete such a folder and run again (re-owning it is not
enough, since its previous owner may still hold it open). New folders and
the staged key are created readable only by SYSTEM and Administrators. If
the earlier `new-host-cert.ps1` was interrupted, its
`host-cert.transaction.json` is still in the directory and both the helper
and the installer stop rather than make a new key: put `host.key` and
`host.crt` back from the `.backup` files it names, delete the journal, and
run again.

Linux deployment installs the helper. The installer itself runs the shared
provisioning plan for the current path during install/upgrade; to invoke the
helper manually:

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
The macOS Pier reloads TLS on `SIGHUP` (`launchctl kill HUP`) in the service
and in `serve`, and reports active, expiring, reloaded, reload-failed and
expired with the same events as the other Piers.
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
renewal; old-binary upgrade; and cross-host validation are validated on the
target platforms, not by the automated suite, and are not claimed by it.
