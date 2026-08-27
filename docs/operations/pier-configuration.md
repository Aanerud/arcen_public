# Unified Pier Configuration

Windows and Linux Piers use the same strict JSON schema for common settings.
The service paths are:

- Windows: `%ProgramData%\Arcen\pier.json`
- Linux: `/etc/arcen/pier.json`

Both packaged services pass that file explicitly with `--config`. Unknown
fields, invalid values, missing files, and missing required policy fields fail
startup. CLI arguments override JSON for diagnostics; `--no-config` is a
CLI-only escape hatch and is not used by packaged services.

## Required audio policy

Every config must contain both objects:

```json
{
  "audio": {
    "enabled": true,
    "compressed": false
  },
  "microphone_input": {
    "enabled": false
  }
}
```

`audio.compressed` is host authority, not a hint:

| Value | Wire codec | Policy |
| --- | --- | --- |
| `false` | PCM | Uncompressed 48 kHz, stereo, signed 16-bit, 20 ms frames; about 1.536 Mbit/s before framing. Intended for LANs where bandwidth is cheap and codec CPU is unnecessary. |
| `true` | Opus | Fixed constrained-VBR 128 kbit/s Opus policy. Intended for bandwidth-constrained links. |

The host advertises only the selected codec. It never silently changes to the
other codec. If the selected codec cannot be negotiated or initialized, audio
is disabled with an actionable codec-unavailable event while video and control
remain connected. Deck may mute audio, but its quality message cannot override
the host codec or configured Opus bitrate.

`microphone_input.enabled` is also host authority. Keep it `false` while input
devices are parked. Linux creates no virtual source while it is false. Windows
does not probe or feed the optional microphone driver while it is false.

## Common sections

The common top-level sections are identical on both hosts:

```json
{
  "listen": {},
  "tls": {},
  "capture": {},
  "video": {},
  "audio": {},
  "microphone_input": {},
  "clipboard": {},
  "auth": {},
  "redirection": {},
  "logging": {},
  "platform": {}
}
```

`listen`, `tls`, `capture`, `video`, `clipboard`, `auth`, `redirection`, and
`logging` are shared schema objects. `platform` is required and contains only
settings whose OS semantics genuinely differ:

- Windows: DXGI desktop/output selection, Windows deskside policy, log rotation
  size, and first-login timeout.
- Linux: PAM mode, dedicated Xorg/session paths, X display/monitor selection,
  disconnected persistent-desktop idle lifetime, uinput, PulseAudio/PipeWire
  helper paths, managed log path, and Linux deskside policy.

Reference templates:

- [`../../packaging/windows/pier.json`](../../packaging/windows/pier.json)
- [`../../packaging/linux/arcen-pier.json`](../../packaging/linux/arcen-pier.json)

Paths in JSON may be absolute. Relative paths are resolved against the
directory containing `pier.json`. TLS keys and credentials are paths only;
secret material must never be embedded in JSON.

### Direct QUIC listener

`listen.port` is the mandatory QUIC UDP port. The packaged templates set it to
`18444`. Pier binds TLS 1.3 with ALPN `arcen-quic-v1` and carries the
authenticated direct-session protocol over one bidirectional QUIC stream.
Resolution, certificate, configuration, or bind failure stops startup; there
is no fallback transport.

The old `listen.quic_port` key is accepted only as a migration alias and wins
over `listen.port` when both are present. New configurations must omit it.

The operating-system firewall must allow inbound UDP on `listen.port`. Current
Linux and Windows installers add `18444/UDP` and remove the legacy
`18443/TCP` rule (the old WSS TCP port) on recognized firewalls. If you are
upgrading from a pre-QUIC deployment, verify the TCP rule is absent. Administrators must audit custom
ports and unrecognized firewall products explicitly.

## Validation and reload

Validate before restart:

```text
arcen-pier validate-config --config <path>
```

Windows headless installers and SSH sessions may use `--schema-only` to validate
strict JSON and TLS material without an interactive-session DXGI probe. Run the
full command from an interactive desktop before approving output selection.

Config precedence is `CLI > JSON > built-in diagnostic defaults`. Packaged
services contain only `--config`, making JSON the single operator surface.
SIGHUP reloads logging verbosity/retention and TLS lifecycle where supported;
media, authentication, session, and device policy changes require a service
restart.

Configs from before this schema are intentionally rejected until migrated.
Add `audio.compressed`, add `microphone_input.enabled`, move Windows-only
settings under `platform`, and replace the Linux systemd argument list with the
packaged JSON template.
