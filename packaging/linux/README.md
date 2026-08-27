# Linux Pier Packaging

`deploy-pier.sh`, `deploy-pier-preflight.sh`, `arcen-pier.service`, and
`arcen-xorg.conf` deploy the active fused `arcen-pier-linux` binary to a Linux
workstation. This is a development deployment path, not a release installer.
Package format, signing, upgrade, provenance, and uninstall behavior remain
Release/Security work.

The deployment builds Pier with embedded NVENC and software-H.264 capenc
support. OpenH264 is compiled from the exact crate-supplied source into the
Pier; no
`libopenh264.so`, downloaded/precompiled Cisco runtime, or nested codec payload
is installed. Deployment checks the binary's dynamic dependencies and release
tree before installation. External distribution still requires the reviewed
H.264 posture, complete target SBOM/notices, and physical acceptance in
`docs/architecture/media-plan-resolution.md`.

Packaged services write their authoritative tracing stream to
`/var/log/arcen/arcen-pier.log` as canonical JSON Lines using the unified
`/etc/arcen/pier.json`. The shipped
logrotate rule renames and creates the file (never `copytruncate`) and sends
`SIGHUP` so the Pier reopens it, re-resolves the active logging profile, and
applies shared 7–100-day archive cleanup. `Xorg.log` remains owned by the
graphical-session lifecycle and is intentionally excluded.

The `logging` section's canonical field is `logging.level` (an
`OperationalProfile` discriminant, `0`–`3`); the packaged template above ships
`0` (`Level0`/`Critical`), the production default. The legacy numeric
`logging.verbosity` is accepted for one release as a migration path and is
mutually exclusive with `logging.level`. The current `pier-linux.example.internal` development
deployment runs with `Level2` (`Info`) for live verification — that override
is intentionally not baked into this packaged template; set `logging.level`
explicitly per-deployment instead. The logrotate policy owns the fixed 32
MiB/daily rotation trigger; there is no ignored runtime size field.

`ARCEN_LOG` remains the highest-priority fine-grained filter. Standalone runs
without `--managed-log` preserve the rolling file plus stderr behavior and
continue honoring `ARCEN_LOG_DIR`.

## TLS certificate lifecycle

The packaged endpoint is direct QUIC on UDP `0.0.0.0:18444`. It uses the
operator-managed PEM from
`/etc/arcen/host.crt` and `host.key`; the private key must be exactly mode
`0600`, and the certificate must be `0644` or stricter. Enterprise
installations should place their complete CA-issued pair there before
deployment. Upgrades never overwrite a complete pair.

`arcen-new-host-cert` is the reviewed SMB helper. OpenSSL and util-linux
`flock` are required by this helper only, not by the running service. `flock`
serializes recovery and publication. With no issuance-mode argument it
generates only when both files are absent and refuses a partial pair. It issues
P-256/SHA-256, 825-day, serverAuth/digitalSignature certificates with explicit
FQDN/DNS and non-loopback IP SANs. Install the helper prerequisite with
`apt install openssl util-linux` on Debian-family hosts or
`dnf install openssl util-linux` on RHEL-family hosts. It also writes:

- `host.cert-sha256`: OpenSSL's `sha256 Fingerprint=AA:...` whole-certificate
  fingerprint.
- `host.spki-sha256`: one `sha256/<base64>` SPKI fingerprint line.
- `host.generated-by-arcen`: pair-bound ownership metadata; renewal and rekey
  refuse if the current certificate, key, and marker no longer agree.

To renew without changing the trusted public key, use exactly:

```sh
sudo /opt/arcen/bin/arcen-new-host-cert --renew --directory /etc/arcen
sudo systemctl reload arcen-pier
```

For a SAN-less pair created by the previous inline Arcen helper, explicitly
adopt it once while preserving its key:

```sh
sudo /opt/arcen/bin/arcen-new-host-cert --renew --adopt-legacy --directory /etc/arcen
sudo systemctl reload arcen-pier
```

Never use `--adopt-legacy` for enterprise/custom PEM. Replace that material
with a SAN-bearing CA-issued pair instead.

An intentional trust-changing rekey must be explicit:

```sh
sudo /opt/arcen/bin/arcen-new-host-cert --new-key --directory /etc/arcen
sudo systemctl reload arcen-pier
```

The helper stages, fsyncs, journals, backs up, and atomically renames files in
the TLS directory; the next invocation recovers an interrupted transaction.
It rejects symlinks in destinations and transaction artifacts. The running
Pier never issues or rotates certificates. `SIGHUP` independently reloads
logging and validates/reloads PEM; either reload can succeed while the other
fails, and invalid material leaves the last good certificate active.

Pier now requires a SAN-bearing leaf with serverAuth-compatible EKU,
digitalSignature key usage when present, a matching admitted key, and current
validity. This is a deliberate strict-SAN rollout break for legacy CN-only
certificates. Use repeatable `--tls-expected-san <DNS|IP>` to require every
operator-selected identity, `--tls-minimum-version TLS1.3`, repeatable
`--tls-disabled-cipher-suite <IANA-NAME>`, and
`--tls-expiry-warning-days <0..3650>`. The shipped QUIC product accepts only
`TLS1.3`; its defaults are TLS 1.3, all three TLS 1.3 ring suites, and 30 days.
During an upgrade, the installer atomically migrates an existing direct-listener
configuration to UDP 18444 and TLS 1.3 and preserves the pre-migration file as
`/etc/arcen/pier.json.pre-quic`.

## Support bundles

The deployment creates `/var/lib/arcen/support` with mode `0700`.
With the Pier service stopped or broken, run:

```sh
sudo arcen-pier support-bundle
sudo arcen-pier support-bundle --out /root/incident
```

The default path never silently falls back; an unavailable default reports an
error and requires `--out`. Bundles are sensitive operational data. Protect and
review them before sharing. They exclude TLS certificate/private-key files and
configured paths, Xauthority, `/run/arcen/sessions`, `Xorg.log`, customer
payloads, and hostnames in filenames or manifests. See
[`docs/operations/support-bundles.md`](../../docs/operations/support-bundles.md).

## Deployment

The deployment installs the Pier binary, TLS helper, configuration, logging
policy, and systemd unit, then starts the service:

```sh
HOST=root@host packaging/linux/deploy-pier.sh
```

The unit uses `Restart=on-failure` with `StartLimitBurst=10` over ten minutes,
so a start that fails for a passing reason is retried for about a hundred
seconds and then given up on.

The unit also declares `After=time-sync.target`, but do not rely on it: that
target is ordering only, and on a host without `systemd-time-wait-sync.service`
— which includes the RHEL family, where `chronyd` does the work — it is reached
well before the clock is correct. The retry is what recovers the race.

## Native lifecycle events (Event Log Lifecycle)

In addition to the file/stderr `tracing` streams above, `arcen-pier` emits a
small set of stable, schema-validated lifecycle records (service
start/stop/failed, session auth outcome, stream start/end/interrupted,
display arm/restore/watchdog outcomes — see `docs/architecture` /
`hosts/linux/ARCHITECTURE.md`) directly to native Linux logging:

- Primary delivery is a cached nonblocking Unix datagram to the systemd
  native journal protocol socket (`/run/systemd/journal/socket`), tagged
  `SYSLOG_IDENTIFIER=arcen-pier` with an `ARCEN_EVENT_ID` field (plus
  `ARCEN_EVENT_NAME`, `ARCEN_CATEGORY`, `ARCEN_OUTCOME`, `ARCEN_SEVERITY`,
  `ARCEN_CORRELATION_ID`, and schema-approved `ARCEN_FIELD_*` entries).
  `arcen-pier.service` also sets `SyslogIdentifier=arcen-pier` so its own
  stderr/stdout journal entries share the same identifier.
- If the journal socket is unavailable, the fallback is one bounded RFC
  3164-style message to `/dev/log` (facility `daemon`, tag `arcen-pier`,
  carrying the same stable `arcen_event_id=<id>` marker).
- If both are unavailable, exactly one structured `tracing` warning reports
  the delivery failure; native delivery never blocks or changes a session,
  auth, or display outcome. No `libsystemd`/`sd-journal` linkage and no new
  logging dependency are used — delivery is plain
  `std::os::unix::net::UnixDatagram`.
- No additional socket permissions, daemon dependencies, or log paths are
  required by packaging.

Query lifecycle records with either:

```sh
journalctl -t arcen-pier -o json | grep ARCEN_EVENT_ID
```

or, if journald itself is unavailable, by grepping the conventional syslog
files for the same marker:

```sh
grep -h 'arcen-pier.*arcen_event_id=' /var/log/syslog /var/log/messages 2>/dev/null
```
