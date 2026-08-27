# Support Bundles

Support bundles provide bounded, service-independent diagnostics for Arcen Pier.
They are available before logging, TLS, SCM/systemd service startup, or the
network server, so a broken host startup does not block collection.

## Create a bundle

Windows (run elevated):

```powershell
& "$env:ProgramFiles\Arcen\Pier\arcen-pier.exe" support-bundle
& "$env:ProgramFiles\Arcen\Pier\arcen-pier.exe" support-bundle --out C:\SecureIncident
```

The default is `%ProgramData%\Arcen\Support`.

Linux (run as root):

```sh
arcen-pier support-bundle
arcen-pier support-bundle --out /root/incident
```

The default is `/var/lib/arcen/support`, mode `0700`.

If a default output location cannot be created or written, collection fails
explicitly and instructs the operator to provide `--out`; there is no fallback.
Archive names are `arcen-support-<unix-seconds>-<pid>[-suffix].zip` and never
contain a hostname. A same-directory partial file is synced and renamed only
after the archive is complete.

## Contents and bounds

Each ZIP ends with `manifest.json` (schema version 1). The manifest indexes and
SHA-256 hashes every payload entry but never hashes itself. Entries, notices,
redactions, files, commands, and native-event records are bounded. File content
is streamed with fixed bounded buffers. Included logs are capped at 200 MiB and
the total payload at 256 MiB; hashes and included sizes cover the transformed
archive bytes, and source truncation is recorded in the manifest.

The allowlists include:

| Source | Windows | Linux |
| --- | --- | --- |
| Arcen logs | Recognized broker, session, archive, and CP diagnostic logs | Recognized managed/standalone Pier logs; preserves `ARCEN_LOG_DIR` |
| Configuration | Redacted `%ProgramData%\Arcen\pier.json` | Redacted `/etc/arcen/pier.json` plus redacted effective systemd configuration |
| Recovery/runtime | Current `recovery::default_path()` display journal | Sanitized service/runtime facts; no persistent display journal |
| Native lifecycle | Bounded sanitized `ArcenPier` Application Event Log XML | Bounded allowlisted journal JSON or hostname-stripped syslog fallback |
| Diagnostics | Service state, OS and display/GPU inventory | Service, OS, process, display/GPU, and bounded approved command output |

Unavailable, denied, invalid, changed, timed-out, or over-limit sources become
typed manifest notices rather than silent omissions. Collection does not invent
unsupported diagnostics; in particular, Windows does not claim a read-only
NVAPI driver query.

## Privacy and handling

Bundles pseudonymize canonical log usernames, hostnames, peer addresses, and
network identities by default with one random, unlinkable per-bundle key.
Repeated identities remain correlated within one bundle, including across log
files, but cannot be correlated between bundles. Malformed, oversized, legacy,
or incomplete log records are omitted with typed manifest notices rather than
copied without transformation. Native lifecycle exports receive the same
identity treatment or are omitted when their safe shape cannot be established.
There is no raw-log export option.

Bundles remain **sensitive operational data**, not anonymous. Pseudonymization
does not remove correlation IDs, operational topology, timing, configuration
posture, or all potentially identifying context. Operators must protect bundles
in storage and transit and review them before sharing.

The collectors exclude credentials, all TLS certificate/private-key files and
configured path values, customer payloads, proprietary payloads, arbitrary
filesystem content, and hostnames in filenames or manifests. Neither TLS path
nor file is statted, opened, hashed, copied, or serialized. Safe bounded
lifecycle metadata may appear only through already-sanitized approved
logs/events; it is never derived by inspecting certificate material. Linux
additionally never traverses `/run/arcen/sessions` (Xauthority) and excludes
`Xorg.log`.

`--out` selects an operator-controlled directory. The collector does not weaken
permissions on a custom directory; create and protect that directory before
collection.

TLS certificate installation, strict-SAN upgrade action, same-key renewal, and
operator reload are documented in
[`tls-certificates.md`](tls-certificates.md).
