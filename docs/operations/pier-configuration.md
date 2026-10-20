# Unified Pier Configuration

Windows, Linux and macOS Piers use the same strict JSON schema for common
settings. The service paths are:

- Windows: `%ProgramData%\Arcen\pier.json`
- Linux: `/etc/arcen/pier.json`
- macOS: `/Library/Application Support/Arcen/pier.json`

## Defaults: everything on, turn off what you don't want

Every installer writes the same values for every common section, and each of
them switches the feature on. The file is there for an administrator to turn
things off. `scripts/check_shared_contracts.py` fails when the three packaged
templates drift apart; only paths and the `platform` section may differ.

| Setting | Default | Notes |
| --- | --- | --- |
| `video` | 10-bit, full range, BT.709, 60 fps, encoder `auto` | Each session still gets what its preset asks for and what the host can encode. |
| `audio.enabled` / `audio.compressed` | on / Opus | Set `compressed` to `false` for PCM on a LAN. |
| `audio.local_playback` | `muted` | The host's own speakers stay silent for the whole session, detached time included, whether or not audio is transmitted. `audible` keeps them playing. See [Local playback](#local-playback). |
| `microphone_input.enabled` | on | Linux creates a virtual source. Windows needs the optional microphone driver; without it the microphone is reported unavailable and the session carries on. macOS does not implement it yet. |
| `clipboard` | both directions, all content | |
| `redirection.timezone` | on | Linux sets `TZ` in the authenticated desktop process tree. Windows temporarily changes the system time zone under a recovery journal. macOS sets `TZ` in the served GUI launchd session for newly launched apps only. |
| Multi-display | on where the host can prove it | Windows resolves the streaming adapter at Pier startup from live DXGI/NVAPI inventory; `allowed_adapters` limits to named GPUs and `excluded_adapters` reserves GPUs for other work. Linux probes the NVIDIA Xorg head roster at service start; `platform.multi_monitor.heads` is only an administrator override. macOS advertises it when it can create virtual displays. `advertise_enabled: false` turns it off everywhere. |

A configuration that is already installed is kept on upgrade; these defaults
apply to new installs.

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
    "compressed": true
  },
  "microphone_input": {
    "enabled": true
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

`microphone_input.enabled` is also host authority. Set it to `false` to refuse
microphone audio from the Deck: Linux then creates no virtual source, and
Windows does not probe or feed the optional microphone driver.

## Local playback

`audio.local_playback` decides whether someone beside the host hears the
remote user. It is separate from `audio.enabled`: a session that transmits no
audio still keeps the room silent by default. With `muted` (the default) a
host that cannot prove its speakers silent **refuses the session** and tells
the Deck "The host could not mute its own speakers"; set `audible` to serve
such a host anyway.

| Host | `muted` | `audible` |
| --- | --- | --- |
| macOS | A Core Audio process tap captures the system output and mutes it; it needs System Audio Recording. | The tap captures without muting. |
| Linux, X11 session | The session agent loads an Arcen session sink in the user's audio server (PulseAudio or `pipewire-pulse`), makes it the default, mutes **every other sink** and moves every stream onto it, and keeps that true for the whole session. Arcen records the session sink. Every change is journalled; at the end only what the lease changed is restored (a sink the user had muted stays muted), and a journal left by a killed agent is restored by the launcher. If the same user is also signed in at the console, that console's speakers are silent during the remote session too. | Nothing in the user's audio server changes; Arcen records the default sink. |
| Linux, Wayland session | The session's audio is a private graph: its own sink and its own PulseAudio-protocol server, with no speaker, so nothing reaches the room; the same check proves it. On a host whose audio server is PulseAudio itself (no `pipewire-pulse`), the session's graph is bridged into the user's PulseAudio, which gets the X11 lease. | The session sink is also played into the user's own default sink. |
| Windows | Not implemented yet. | — |

A Linux host with neither an audio server nor a sound card is silent by
construction and is served under `muted`.

**Upgrading a Linux host.** Linux Piers before this release ignored the
setting. An existing configuration without `local_playback` now gets the
default, `muted`: the host's speakers go silent during sessions, and a host
that cannot prove it refuses them. Set `"local_playback": "audible"` to keep
the previous behaviour.

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
