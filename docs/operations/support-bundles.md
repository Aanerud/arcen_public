# Support Bundles

Support bundles provide bounded, service-independent diagnostics for Arcen Pier.
They are available before logging, TLS, SCM/systemd service startup, or the
network server, so a broken host startup does not block collection.

Every Pier and both Decks write their bundle with one collector,
`arcen_observability::support_bundle`: the archive, its limits, safe opening
of sources, canonical-log export with per-bundle pseudonyms, configuration
redaction and the private, atomically published output file are the same
code everywhere. Each product supplies only its list of sources.

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

macOS (in development; run as the Pier's user or root):

```sh
arcen-pier-macos support-bundle
arcen-pier-macos support-bundle --out /Users/Shared/incident
```

The default is `/Library/Logs/Arcen/Pier/support`, mode `0700`. Archive names
are `arcen-pier-macos-support-<unix-seconds>[-suffix].zip`.

If a default output location cannot be created or written, collection fails
explicitly and instructs the operator to provide `--out`; there is no fallback.
Archive names are `arcen-support-<unix-seconds>-<pid>[-suffix].zip` and never
contain a hostname. A same-directory partial file is synced and renamed only
after the archive is complete.

**Crash reports** are left out of every bundle unless asked for:
`--include-crash-dumps` on every Pier and Deck command line, or the
**Include crash reports** checkbox in a Deck's panel (off by default). A crash
report's stack can hold what the program was handling at the time, a sign-in
password included, and no redaction can find that reliably. The manifest
records the choice under `crash`: `crash_reports_not_requested` (left out),
`crash_reports_requested` (included), or `crash_reports_not_collected` (asked
for, but the product does not collect crash reports yet; today every Pier, see
GAP-040 in the platform-differences ledger).

Every manifest carries a random `bundle_id` (`ARC-XXXX-XXXX`). It identifies
the bundle in a public report without revealing anything about it.

## Contents and bounds

Each ZIP ends with `manifest.json` (schema version 1). The manifest indexes and
SHA-256 hashes every payload entry but never hashes itself. Entries, notices,
redactions, files, commands, and native-event records are bounded. File content
is streamed with fixed bounded buffers. On every Pier, included logs are capped
at 200 MiB and the total payload at 256 MiB, and each log is read from its
tail, so the newest records are kept; hashes and included sizes cover the
transformed archive bytes, and source truncation is recorded in the manifest.
Entries are private when extracted on Unix (`0600`) and carry the bundle's
time.

The allowlists include:

| Source | Windows | Linux | macOS |
| --- | --- | --- | --- |
| Arcen logs | Recognized broker, session, archive, and CP diagnostic logs | Recognized managed/standalone Pier logs; preserves `ARCEN_LOG_DIR` | The managed `arcen-pier-macos.jsonl`; preserves `ARCEN_LOG_DIR` |
| Configuration | Redacted `%ProgramData%\Arcen\pier.json` | Redacted `/etc/arcen/pier.json` plus redacted effective systemd configuration | The logging profile and platform switches of `pier.json` |
| Recovery/runtime | Current `recovery::default_path()` display journal | Sanitized service/runtime facts; no persistent display journal | — |
| Native lifecycle | Bounded sanitized `ArcenPier` Application Event Log XML | Bounded allowlisted journal JSON or hostname-stripped syslog fallback | — (the managed log carries the lifecycle records) |
| Diagnostics | Service state, OS and display/GPU inventory | Service, OS, process, display/GPU, and bounded approved command output; the desktop provider the Pier selects at start (`diagnostics/desktop-provider.json`: preference, provider and reason, or the refusal with its blockers, and the qualification record path) and the configured Wayland qualification record itself (`diagnostics/wayland-qualification.json`, redacted like configuration) | Permissions and display inventory (`diagnostics/report.json`) |

`config/pier.json` is redacted the same way on every Pier: secret-bearing keys,
the private key (recorded as the private-key policy), and the TLS certificate
path and expected SANs, which name the customer's host.

Unavailable, denied, invalid, changed, timed-out, or over-limit sources become
typed manifest notices rather than silent omissions. Collection does not invent
unsupported diagnostics; in particular, Windows does not claim a read-only
NVAPI driver query.

## Privacy and handling

Bundles pseudonymize canonical log usernames, hostnames, peer addresses, and
network identities by default with one random, unlinkable per-bundle key:
the record's `user`, `host` and `peer_addr`, and the `fields` products log the
same identities under: the names `username`, `hostname`, `server`,
`server_name`, `addr`, `local_addr`, `peer`, `remote_addr`, `endpoint` and
`ssid`, and any field named like one (`…_user`, `…_username`, `…_account`,
`…_addr`, `…_address`, `…_addresses`, `…_host`, `…hostname`, `…_domain`,
`uri`, `url`, `…_uri`, `…_url`, `…_endpoint`, and `resolved`). Each replaced key path is listed
in the manifest's redactions. An IPv4 or IPv6 literal anywhere else in a
record's text, `message` included (a dependency's error naming a
destination), gets the same address pseudonym. So does the account name of
a home folder anywhere in that text: it gets the record `user` pseudonym, so
the path keeps its shape and correlates with the account's other records.
That covers Windows (`C:\Users\<name>\…` in any case, with backslashes,
forward slashes or JSON-escaped ones, and `Documents and Settings`), macOS
(`/Users/<name>/…`) and Linux (`/home/<name>/…`); `C:\Users\alice\AppData\Local\Arcen`
becomes `C:\Users\anon:<hex>\AppData\Local\Arcen`. System folders
(`Public`, `Default`, `All Users`, `Shared`) stay. Other free text is not
rewritten: Arcen's messages do not name accounts or host names, which is what
keeps those out.

One rule covers a machine's name or address in every place above, on every
Pier and Deck, and in a Deck's saved connections (`host`, and the connection
IDs, which older Decks derived from the host), its trusted pins (the
`host:port` keys) and its network probe (`target`). The host
part becomes the bundle's pseudonym for that machine, so the same machine
reads the same in every file. An IP literal keeps only its family and scope,
and a port stays: `203.0.113.82:18444` becomes `anon:<hex>[ipv4-public]:18444`,
and a host name becomes `anon:<hex>[name]`. The scopes are `loopback`,
`private`, `link-local`, `shared` (100.64/10), `ula`, `multicast`,
`unspecified` and `public`.

Repeated identities remain correlated within one bundle, including across log
files, but their pseudonyms cannot be correlated between bundles. Values that
are not pseudonymised can be: a Deck's trusted certificate fingerprints, its
connection names and display models are the same in every bundle. Malformed, oversized, legacy,
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

The desktop-provider report runs the same start-time selection as the
service, from host facts only (it never starts a session), for the Pier
executable that takes the bundle (`evaluated_by`). A qualification record is
valid for one Pier executable, so a bundle taken with another build than the
installed service reports the record as stale; take bundles with the
installed `/opt/arcen/bin/arcen-pier`.

TLS certificate installation, strict-SAN upgrade action, same-key renewal, and
operator reload are documented in
[`tls-certificates.md`](tls-certificates.md).

## Deck support bundles

Both Decks (macOS and Windows) make a bundle of their own with the same
schema-v1 manifest, from **Settings › Logs › Create support bundle…** or the
command line:

```sh
arcen-deck support-bundle                 # saves to the Desktop
arcen-deck support-bundle --out ~/incident
```

```powershell
& "$env:ProgramFiles\Arcen\Deck\arcen-deck.exe" support-bundle --out C:\Incident
```

Where each Deck keeps what the bundle reads (`ARCEN_CONFIG_DIR` and
`ARCEN_LOG_DIR` override the first two):

| | macOS Deck | Windows Deck |
| --- | --- | --- |
| Settings, saved connections, trusted pins | `~/Library/Application Support/Arcen` | `%APPDATA%\Arcen\Deck` (`C:\Users\<name>\AppData\Roaming\Arcen\Deck`) |
| Logs (`arcen-client.log.<date>`) | `~/Library/Logs/Arcen` | `%LOCALAPPDATA%\Arcen\Deck\Logs` (`C:\Users\<name>\AppData\Local\Arcen\Deck\Logs`) |
| Crash reports | `~/Library/Logs/DiagnosticReports` | `%LOCALAPPDATA%\Arcen\Deck\CrashDumps` (WER LocalDumps) |

The archive is `arcen-deck-support-<UTC time>.zip` (never a hostname), written
through a partial file that is renamed when complete. After it is written the
panel offers **Copy path** and **Show in folder**. The bundle is built by
shared code (`arcen_deck_core::support_bundle` on the shared collector); each
Deck only gathers what its operating system knows. A Deck reads at most the
32 MiB tail of each daily log (128 MiB in all) because it holds each log in
memory to remove the computer's account name, which a Pier's logs never carry.

| Entry | Contents |
| --- | --- |
| `deck/about.json` | Build identity, OS and version, the log level, the config and log folders (home folder shown as `~`) |
| `logs/arcen-client.log.<date>` | The newest seven daily Deck logs at the level they were written, canonical records only, the tail of a long day (32 MiB per file, 128 MiB in all) |
| `config/settings.json`, `config/connections.json`, `config/trusted_pins.json` | Settings, saved connections and trusted certificate fingerprints, redacted |
| `diagnostics/displays.json` | Displays, arrangement and colour facts (gamut, HDR headroom); no EDID and no serial number |
| `diagnostics/decode.json` | What the hardware decoder proved, the GPU that answered, and why each refused profile was refused |
| `diagnostics/network.json` | Interface kind, scope and MTU of the path to the host of the last session (also an unsaved `--connect` one), else the last selected connection |
| `diagnostics/platform.json` | macOS: Mac model, CPU, GPU, decode path. Windows: every GPU with PCI ids, memory and driver version, the Windows build, crash-dump registration |
| `diagnostics/native-usb-component.json` | The Native tablet helper (macOS) or USB component (Windows) state |
| `session/last-session.json` | The last session's Stream Statistics, Negotiated Truth, health overlay and telemetry counters |
| `crash/*` | Only when asked for: the five newest Deck crash reports, macOS `arcen-deck-*.ips`, Windows `arcen-deck*.dmp` (WER minidumps) |

Redaction, on top of the Pier rules above:

- Any key the shared policy calls secret-bearing (`password`, `token`, `key`,
  `secret`, …) is replaced by `<redacted>`; only the Deck's keyboard settings
  that merely contain such a word (`option_key_mode`, `keyboard_shortcuts`) are
  exempt by exact name. Passwords are never saved by a Deck in the first place.
- Saved usernames and Wi-Fi names become per-bundle pseudonyms, from the same
  key as the log identities, so they correlate within one bundle only. Host
  names and addresses follow the address rule above, in the logs, the saved
  connections, the trusted pins and `diagnostics/network.json`. Connection
  names and pin labels stay as the user typed them.
- The user's home folder becomes `~` in every string, and the account name
  (and the home folder's own name) becomes `<user>` wherever it is a path
  component; this happens before the shared log transform, so in the logs
  only another account's home folder gets the account pseudonym above. In minidumps the account name and the computer's name are
  overwritten in place with `*` wherever they stand as a whole name, and so
  are the values of identity and secret variables in the dump's environment
  block (`USERNAME`, `COMPUTERNAME`, `PROMPT`, anything named like a token,
  key, secret or password).
- A minidump that holds process memory beyond thread stacks (full, private,
  write-copy or indirectly referenced memory) is left out with a notice.
  Thread stacks can still hold fragments of what the Deck was doing at the
  crash; review a bundle before sharing it.
- Clipboard contents, credentials and typed text are never collected; the
  manifest lists them as omitted.

## Public summary and sealed bundles

A bundle is never meant for a public place, such as an issue tracker. Two
outputs of the shared collector (`arcen_observability::support_bundle`) let a
reporting flow keep it out of one:

- **Public summary** (`PublicSummary`): Markdown made from fixed, typed fields
  only. It never redacts anything out of log text. It states the bundle ID,
  the archive's SHA-256 and size, the product, version, OS family and
  architecture, the release artefact's SHA-256 (never the private build ID or
  source revision), the OS, GPU and decoder names, what the decoder proved,
  the last session's preset, error codes from the error-code reference, entry
  counts and bytes per top-level section (`logs`, `config`, `diagnostics`, …),
  notice and redaction codes with counts, and the product's typed facts. A Deck
  adds the number of displays and crash reports, whether crash reports are
  in, and the last session's counters and worst health state. It has no time,
  log lines, settings, archive file names, redacted key paths, accounts, hosts
  or addresses. A word that is not a plain product word (ASCII letters,
  digits, `.`, `_`, `+` or `-`), a label (OS, GPU, decoder) with a path,
  address or dotted host name, or a code outside that shape is printed as
  `<withheld>`. `to_public_text` then refuses the whole text if it still holds
  an address, a dotted host name, a home folder, or the account or computer
  name the caller passes.
- **Sealed bundle** (`write_sealed_archive`, on a Deck `create_sealed`): the
  full bundle is encrypted by a `BundleSealer` (for example to the project's
  public key) before it is published as `<name>.zip.<extension>`. The
  plaintext archive exists only as a private partial file in the same folder
  while it is written, and is removed when writing ends, whether it succeeded
  or failed (a process killed mid-write can leave it, like any partial
  file). The summary carries the plaintext archive's SHA-256, so a maintainer
  who opens the sealed file can match it to the public report.

The collector does not choose a cipher or a recipient key and does not send
anything; that belongs to the reporting flow.
