# Arcen Pier administration guide

Audience: sysadmins operating large Linux and Windows Pier deployments. This guide describes the direct QUIC Pier to macOS Deck product scope. Gateway or Span, Windows Deck, Linux Deck, accounts portal, and commercial licensing services are not available in this build.

Verification note: every command, path, config key, default, and failure string below is tied to source files in this repository. Items marked **target state, not verified in this branch** describe the single-binary installer work that is still in flight.

## 1. What Arcen is

Arcen has two active operator surfaces today:

| Surface | Active product | What it does |
| --- | --- | --- |
| Host | Arcen Pier on Linux and Windows | Listens on direct QUIC UDP 18444, authenticates a user, mutates or owns a desktop, launches a user-session agent, captures and encodes the display, forwards audio and clipboard policy, and injects input. |
| Client | Arcen Deck on macOS | Connects directly to one Pier over QUIC, authenticates, decodes media, renders the desktop, and forwards input. |

Installing Pier changes the blast radius of the workstation. Linux Pier runs as root, uses PAM, creates a dedicated Xorg session, starts user-context helpers, and can inject input through uinput. Windows Pier runs as LocalSystem, uses `LogonUserW`, can drive an additive Credential Provider at LogonUI for first login, mutates display topology under a recovery journal, launches a user-session agent on `winsta0\default`, and injects input with `SendInput`.

Arcen Span gateway, multi-stream/datagram QUIC optimization, federated identity, MFA, Windows Deck, Linux Deck, macOS Pier, and web account management are roadmap or dormant in this repository. Do not plan production procedures around them.

## 2. Installation

### Linux

Current verified deployment is a development deployment script, not a release installer:

```sh
HOST=root@host packaging/linux/deploy-pier.sh
```

The script builds on the target, installs runtime files, validates `/etc/arcen/pier.json`, installs systemd, enables the service, and starts the unit.

| Item | Verified current path |
| --- | --- |
| Pier binary | `/opt/arcen/bin/arcen-pier` |
| Helper model | **One binary.** `arcen-pier` is multi-call: `capenc`, `audiocap`, `input-helper`, `session-agent`, `session-launcher` and `new-host-cert` are subcommands of the same executable. There are no separate helper files. |
| Helper isolation | Unchanged. Helpers still run as separate **processes**, spawned via `current_exe()` plus the subcommand. Only the file count dropped, not the process boundary. |
| Config | `/etc/arcen/pier.json` |
| TLS | `/etc/arcen/host.crt`, `/etc/arcen/host.key` |
| Logs | `/var/log/arcen/arcen-pier.log` |
| Log rotation | `/etc/logrotate.d/arcen-pier` |
| Support bundles | `/var/lib/arcen/support` |

The script creates `/opt/arcen/bin`, `/usr/share/doc/arcen`, `/etc/arcen`, `/var/log/arcen`, and `/var/lib/arcen/support`. It installs `/etc/systemd/system/arcen-pier.service` with `User=root`, `Restart=on-failure` bounded by `StartLimitBurst=10` over ten minutes, ordering after `time-sync.target`, and `ExecReload=/bin/kill -HUP $MAINPID`.

`install_arcen_pier --uninstall` removes the binary and the systemd unit and
leaves `/etc/arcen`, `/var/lib/arcen` and the logs in place. The manual
equivalent, for hosts installed before the installer existed, is:

```sh
sudo systemctl stop arcen-pier
sudo systemctl disable arcen-pier
sudo rm -f /etc/systemd/system/arcen-pier.service
sudo systemctl daemon-reload
sudo rm -f /opt/arcen/bin/arcen-pier
```

Leave `/etc/arcen`, `/var/lib/arcen/support`, and `/var/log/arcen` in place unless a decommission procedure explicitly preserves and then deletes operational data.

### Windows

Installation is a single `install-arcen-pier.exe`, which embeds both the Pier
and the credential-provider DLL. It creates the ProgramData layout, applies and
then **verifies** the ACLs, provisions TLS, registers the service and the
credential provider, registers the owned `ArcenPier` Application event source,
and keeps a rollback copy.

On upgrade, both platform installers preserve unrelated operator settings while
atomically normalizing the direct listener to QUIC/UDP 18444, removing the
legacy `listen.quic_port` alias, and raising the TLS floor to TLS 1.3. Linux
keeps `/etc/arcen/pier.json.pre-quic`; Windows retains the previous file under
`%ProgramData%\Arcen\rollback`.

Fresh installations apply a fail-closed multi-monitor safe-auto policy:

- Windows first installs the trusted embedded Pier, then runs its read-only
  `diagnose-host --json` and `nvapi-inventory --json` commands. It enables
  native NVIDIA headless multi-monitor only when exactly one NVENC-capable
  Quadro/GRID adapter matches across both inventories and exposes at least two
  display IDs. The generated config names that exact adapter, enables the
  hardware-validated two-display ceiling and optional software overflow, and
  leaves NVENC capacity to measured runtime admission.
- A Windows host with no eligible adapter, more than one eligible adapter, an
  incomplete inventory, or a failed probe remains disabled. The installer
  prints the exact reason. In particular, it never guesses which GPU a
  multi-GPU host reserves for non-Arcen work.
- Linux remains disabled on a fresh install because the installer cannot prove
  the NVIDIA DFP head roster without creating a temporary X/NV-CONTROL
  session. It does not mutate the display server merely to guess a default and
  prints the `platform.multi_monitor.heads` action the operator must take. The
  packaged JSON may omit `platform.multi_monitor`; omission is the disabled
  default, not an incomplete installation.

Safe-auto runs only when the config does not exist. Upgrades retain the
operator's complete `platform.multi_monitor` policy; the listener/TLS schema
migration described above does not replace that section.

Both installers register and enable the Pier service but deliberately leave it
stopped when no valid license is installed. Install the license and then start
the service using the command printed by the installer. This avoids consuming
the service manager's bounded restart budget while the expected offline
licensing step is still outstanding.

The ACL verification asserts the exact expected principal set for each
directory class and fails closed on empty or malformed `icacls` output. Secret
directories (`tls`, `licenses`), `pier.json` and `host.key` are restricted to
`BUILTIN\Administrators` and `NT AUTHORITY\SYSTEM` with no `Users` entry.

| Item | Verified current path or name |
| --- | --- |
| Pier binary | `%ProgramFiles%\Arcen\Pier\arcen-pier.exe` |
| Helper model | **One binary plus one DLL.** `arcen-pier.exe` is multi-call; `capenc` is a subcommand of it. |
| Credential Provider DLL | `arcen_credential_provider.dll`, alongside the exe. This cannot be folded into the exe: Windows must load a credential provider as a registered COM in-proc server, so it has to exist as its own file. |
| Config | `%ProgramData%\Arcen\pier.json` |
| TLS | `%ProgramData%\Arcen\tls\host.crt`, `%ProgramData%\Arcen\tls\host.key` |
| Logs | `%ProgramData%\Arcen\logs\arcen-pier.log`, `%ProgramData%\Arcen\logs\sessions\arcen-session-agent-<sid>.log` |
| Service | `ArcenPier`, display name `Arcen Pier`, LocalSystem, Automatic |
| Event source | `ArcenPier` under Windows Application log. The single-file installer embeds and invokes `%ProgramData%\Arcen\eventlog-source.ps1`; the source is changed or removed only when `ArcenOwned=arcen-pier-windows` matches. Registration/removal failure is a visible warning, not a service-install failure; protected JSONL remains authoritative. |
| Support bundles | `%ProgramData%\Arcen\Support` |

### Reinstalling, and what each flag does not do

Reinstalling over an existing host preserves state on purpose, which is
occasionally the opposite of what an operator expects:

| Command | Replaces | Leaves alone |
| --- | --- | --- |
| `install-arcen-pier.exe` | binaries, ACLs, service registration | `pier.json`, TLS material |
| `install-arcen-pier.exe --force` | additionally the TLS certificate and key | **`pier.json`** |
| `install-arcen-pier.exe --uninstall` | binaries, service, event source | all of `%ProgramData%\Arcen` |
| `install-arcen-pier.exe --uninstall --purge` | the above plus `%ProgramData%\Arcen` | a dated copy of `pier.json` |

`--purge` copies `pier.json` to `C:\ProgramData\arcen-pier.json.purged-<epoch>`
before it deletes the tree, and the Linux installer copies `/etc/arcen/pier.json`
to `/etc/arcen-pier.json.purged-<epoch>`. The configuration is the one thing on
a Pier the installer cannot reconstruct — GPU pinning, monitor layout and
transport tuning are site decisions, and `safe_auto` deliberately refuses to
pin an adapter on an ambiguous multi-GPU host. Purging and reinstalling such a
host therefore moves streaming onto whichever adapter enumerates first unless
the pin is restored. Copy the preserved file back before starting the service,
or re-apply the pin by hand. A failed copy warns but does not stop the purge.

`--force` is about TLS, typically after `--extra-san`. It does **not** rewrite
the configuration, so it cannot be used to escape a bad `pier.json`.

To reset only the configuration, delete it and reinstall; the installer writes a
fresh default whenever the file is absent, and no reboot is involved:

```powershell
Remove-Item 'C:\ProgramData\Arcen\pier.json' -Force
.\install-arcen-pier-windows-x86_64.exe
```

`--purge` can report that the Credential Provider DLL is still loaded by LogonUI
and needs a reboot. Installers built before that behaviour was corrected stopped
at that point *without* removing `%ProgramData%\Arcen`, so a configuration the
operator was purging in order to escape survived into the next install. If the
uninstall mentions a reboot, confirm the directory is actually gone before
trusting that the machine is clean.

The manual service command is:

```powershell
$pierExe = Join-Path $pierRoot 'arcen-pier.exe'
$binaryPath = "`"$pierExe`" service --config `"$configPath`""
New-Service -Name ArcenPier `
  -BinaryPathName $binaryPath `
  -DisplayName 'Arcen Pier' `
  -Description 'Arcen direct remote-workstation host' `
  -StartupType Automatic
sc.exe failure ArcenPier reset= 86400 actions= restart/5000/restart/15000/none/0
New-NetFirewallRule -DisplayName 'Arcen Pier QUIC 18444' `
  -Direction Inbound -Action Allow -Protocol UDP -LocalPort 18444
```

Windows uninstall and rollback are manual and must avoid other Credential Providers:

```powershell
Stop-Service ArcenPier -ErrorAction SilentlyContinue
sc.exe delete ArcenPier
Remove-NetFirewallRule -DisplayName 'Arcen Pier QUIC 18444' -ErrorAction SilentlyContinue
..\..\packaging\windows\host\eventlog-source.ps1 -Uninstall
```

Credential Provider uninstall uses the generated CP `uninstall.ps1` from the signed or test artifact set. Reboot after removing a loaded CP DLL.

## 3. Configuration reference

Both platforms use strict JSON with `deny_unknown_fields`. Unknown keys fail parsing. `audio`, `microphone_input`, and `platform` are required. Paths may be absolute. Relative `tls`, `capture`, and disclaimer paths resolve relative to `pier.json`; Linux also resolves Linux platform paths relative to `pier.json`.

Packaged services pass only `--config`, so the JSON file is the operator surface. CLI overrides exist for diagnostics and validation.

### Common schema keys

| Key | Type | Default | Applies to | What it does | Takes effect |
| --- | --- | --- | --- | --- | --- |
| `listen.host` | string | Linux built-in `127.0.0.1`, Linux package `0.0.0.0`; Windows `0.0.0.0` | Both | Bind address. | Restart |
| `listen.port` | nonzero u16 | `18444` | Both | Mandatory direct QUIC UDP port. Bind failure stops startup. | Restart |
| `listen.quic_port` | u16 or absent | absent | Both | Deprecated migration alias for the QUIC UDP port; new configs must omit it. | Restart |
| `tls.mode` | string | unset or `pem` | Both | Only PEM files are supported. | Restart or TLS reload |
| `tls.cert`, `tls.certificate` | path string | package path | Both | PEM certificate chain. Alias `certificate` maps to `cert`. | Restart or TLS reload |
| `tls.key`, `tls.private_key` | path string | package path | Both | PEM private key. Alias `private_key` maps to `key`. | Restart or TLS reload |
| `tls.minimum_version` | `TLS1.3` | `TLS1.3` | Both | Shipped QUIC requires TLS 1.3. | Restart or TLS reload |
| `tls.disabled_cipher_suites` | array of suite names | `[]` | Both | Blacklists the three rustls/ring TLS 1.3 suites. | Restart or TLS reload |
| `tls.expiry_warning_days` | u64 | `30` | Both | Warning window before certificate expiry. | Restart or TLS reload |
| `tls.expected_sans` | array of strings | `[]` | Both | Extra exact DNS/IP SANs that must appear in the leaf. | Restart or TLS reload |
| `capture.binary` | path string | unset | Both | Legacy external-helper path; accepted with a warning and ignored because Pier always spawns its own `capenc` subcommand. | Restart |
| `video.codec` | string or absent | absent in packages | Both | Optional exact administrator codec pin: `h264`, `h265`/`hevc`, or `av1`. Omit for automatic client-capability/hardware selection. | Restart |
| `video.chroma` | string or absent | absent in packages | Both | Legacy host default (`yuv420` or `yuv444`). Use `video.variant` when the full format must be pinned exactly. | Restart |
| `video.fps` | u32 | Linux built-in `60`, Windows `30` | Both | Target FPS, 1 through 240. | Restart |
| `video.encoder` | string | `auto` | Both | `auto`, `nvenc`, or `software-h264` (`openh264` alias on Windows). `nvenc` and `software-h264` are exact backend pins. | Restart |
| `video.bit_depth` | string | Built-in `8`; packages `10` | Both | Ceiling coded component depth: `8`, `10`, or `12`. This is a policy ceiling under `video.color_policy`'s `default-*` values (see below), and an absolute value under its `always-*` values. Packaged hosts expose the proven 10-bit ceiling without forcing it. `12` requires the software encoder tier: NVENC has no 12-bit mode at any subsampling. | Restart |
| `video.color_range` | string | Built-in `limited`; packages `full` | Both | Ceiling coded sample range: `limited` or `full`. Existing hand-written deployments that omit it retain `limited`; packaged hosts expose the full-range ceiling under `default-off`. | Restart |
| `video.color_matrix` | string | `bt709` | Both | Ceiling matrix coefficients used to derive luma/chroma from RGB: `identity`, `bt709`, `bt601`, or `bt2020ncl`. | Restart |
| `video.color_policy` | string | `default-off` | Both | `always-on`, `always-off`, `default-on`, or `default-off`. Governs how `bit_depth`/`color_range`/`color_matrix` interact with a negotiating client. See "Colour fidelity policy" below. | Restart |
| `video.variant` | string or absent | absent | Both | Strongest exact administrator pin: a complete probe-matrix variant id such as `hevc-444-10-full-bt709`. It overrides the individual format keys and the Deck's automatic codec choice. | Restart |
| `audio.enabled` | bool | Required. Linux built-in `false`, packages `true`; Windows `true` | Both | Enables host-to-Deck audio. | Restart |
| `audio.compressed` | bool | Required. `false` in packages | Both | `false` selects PCM; `true` selects fixed Opus policy. | Restart |
| `microphone_input.enabled` | bool | Required. `false` | Both | Enables optional Deck microphone publication policy. | Restart |
| `clipboard.direction` | string | policy default `both`; packages `both` | Both | `both`, `client_to_host`, `host_to_client`, or `disabled`. | Restart |
| `clipboard.content` | string | policy default `all`; packages `all` | Both | `all`, `text`, or `image`. | Restart |
| `clipboard.max_bytes` | usize | policy default and packages `8388608` | Both | Encoded clipboard cap. Valid 1 MiB through 20 MiB. | Restart |
| `auth.disclaimer.enabled` | bool | `false` | Both | Requires disclaimer acknowledgement before OS authentication. | Restart |
| `auth.disclaimer.locale` | string | `en_US` | Both | Locale file stem. | Restart |
| `auth.disclaimer.directory` | path string | Linux `/etc/arcen/disclaimers`; Windows `disclaimers` relative to config | Both | Directory containing `<locale>.txt`. | Restart |
| `auth.reconnect_window_secs` | u32 | `180` | Both | Direct resume window. Valid shared range is 0 through 7200. Zero disables resume. The host keeps its display authority for the whole window, so this is also how long another user waits after somebody disconnects. | Restart |
| `redirection.timezone` | bool | `false` | Both | Linux sets `TZ` in authenticated desktop process tree. Windows temporarily changes machine time zone under journal. | Restart |
| `logging.level` | integer | `0` | Both | Operational profile: 0 Critical, 1 Error, 2 Info, 3 Debug. | Linux SIGHUP, Windows control 201, restart |
| `logging.verbosity` | integer | unset | Both | Legacy one-release mapping: 0→Error, 1→Info, 2/3→Debug. Mutually exclusive with `logging.level`. | Linux SIGHUP, Windows control 201, restart |
| `logging.retention_days` | u16 | `30`, normalized 7 through 100 | Both | Log archive retention. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.fps_degraded_percent` | u8 | `90` | Both | Degraded FPS percent threshold. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.fps_critical_percent` | u8 | `70` | Both | Critical FPS percent threshold. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.rtt_degraded_ms` | u32 | `60` | Both | Degraded RTT threshold. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.rtt_critical_ms` | u32 | `150` | Both | Critical RTT threshold. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.drop_degraded_basis_points` | u16 | `50` | Both | Degraded drop threshold. 50 is 0.5%. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.drop_critical_basis_points` | u16 | `500` | Both | Critical drop threshold. 500 is 5%. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.input_degraded_ms` | u32 | `50` | Both | Degraded input latency. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.input_critical_ms` | u32 | `120` | Both | Critical input latency. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.heartbeat_critical_misses` | u32 | `3` | Both | Missed-heartbeat critical threshold. | Linux SIGHUP, Windows control 201 |
| `platform` | object | required | Both | Platform-specific section below. | Depends on child key |

TLS cipher-suite names accepted in `tls.disabled_cipher_suites` are exactly: `TLS13_AES_256_GCM_SHA384`, `TLS13_AES_128_GCM_SHA256`, `TLS13_CHACHA20_POLY1305_SHA256`, `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`, `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`, `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`, `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`, `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`, and `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256`.

QoS validation rules: FPS degraded must be 100 or lower and greater than FPS critical; degraded RTT must be nonzero and less than critical RTT; degraded drop basis points must be less than critical and critical must be at most 10000; degraded input must be nonzero and less than critical; heartbeat critical misses must be nonzero.

### Automatic Deck policy and administrator overrides

Production Deck users choose only **Performance Mode** and **Colour Fidelity**.
They never select codecs. Standard colour authorizes the host to rank AV1,
HEVC, then H.264 on the approved adapter before OpenH264. Full Colour requests
4:4:4 8-bit. Grading Reference requests 4:4:4 10-bit and automatically selects
the quality encoder intent.

Packaged Piers leave `video.codec` and `video.chroma` absent so this automatic
policy is active. Advanced administrators may set:

```json
{
  "video": {
    "codec": "h265",
    "encoder": "nvenc"
  }
}
```

Those values are exact pins, not preference hints. A codec pin retains the
Deck's requested colour axes; an incompatible combination (for example AV1
with Grading Reference 4:4:4) is rejected instead of silently choosing another
codec. Backend loss cannot bypass a pin: a pinned AV1/HEVC request is rejected
instead of becoming HEVC/H.264/OpenH264. A pinned H.264 request may use
OpenH264 because the codec remains H.264. `video.variant` pins the complete
format and is stronger than `video.codec`; an exact OpenH264 variant must also
fit its 30 fps ceiling at config-validation time. Every change requires a Pier
restart.

### Colour fidelity policy (`video.color_policy`)

`video.bit_depth`, `video.color_range`, and `video.color_matrix` set a
**ceiling and default**, not by themselves an absolute value: the actual
per-session bit depth, colour range, and matrix are resolved against what a
negotiating Deck client asks for, and `video.color_policy` governs exactly how
much say the client gets. This mirrors Amazon DCV's
`display/enable-yuv444-encoding` parameter (`always-on` / `always-off` /
`default-on` / `default-off`), adapted to Arcen's negotiate-best model, where
config is normally a ceiling rather than a forced value:

Current Decks send `AuthResponse.initial_video` with Performance/Colour
Fidelity intent and measured decode capabilities before the Pier creates its
display/encoder. Performance preserves these colour axes while ranking AV1,
HEVC, and H.264 on the operator-approved adapter. Full Colour and Grading
Reference request HEVC 4:4:4. The first `ServerHello` is therefore the final
plan; the subsequent `quality_settings` message is a consistency echo. Legacy
clients that omit `initial_video` retain the single-monitor late-request path.
A legacy multi-monitor request that would change codec, colour, or encoder
intent is rejected with an explicit reconnect/upgrade reason; it is never
silently served with a stale Interactive contract.

Matrix negotiation is capability-gated just like depth and range. Current
Decks advertise BT.601 and BT.2020 NCL support independently. Older Decks
advertise neither, so a Pier caps them to BT.709 instead of applying an
unrecognised matrix.

| `video.color_policy` value | Behaviour |
| --- | --- |
| `always-on` | Forces the configured `bit_depth`/`color_range`/`color_matrix` ceiling for every session. A client asking for less is ignored. |
| `always-off` | Forces the conservative baseline (8-bit, limited range, BT.709) for every session, regardless of `bit_depth`/`color_range`/`color_matrix`. A client asking for more is ignored. |
| `default-on` | Defaults to the configured ceiling, but an explicit client request is honoured up to that ceiling (a client asking for less gets less). |
| `default-off` (default) | Defaults to the conservative baseline, but an explicit client request is honoured up to the configured ceiling (a client asking for more gets more, capped at the ceiling). This is the only value that keeps an existing deployment's behaviour unchanged when the other three keys are left at their own defaults. |

Only `always-on` and `always-off` are absolute overrides of the client. Under
either `default-*` value, the configured keys bound how far a session can go;
they never force a client that asked for less to receive more, or the reverse.

`video.variant` is the fast path for pinning one exact row of the coded-format
probe matrix (codec, chroma, bit depth, range, and matrix together), for
example on a colour-grading test host. Setting it overrides `video.codec`,
`video.chroma`, `video.bit_depth`, `video.color_range`, and
`video.color_matrix` regardless of what those keys are also set to, forces the
session to exact selection, and applies the pinned contract from the first
`ServerHello`. A variant row that is internally coherent but selects a format
this host does not run (for example AV1 4:4:4 or a 4:2:2 row) is rejected with
a clear error at config-validation time rather than accepted and left to fail
later at session start.

**12-bit is unavailable on NVENC.** No NVIDIA GPU encodes 12-bit at any chroma
subsampling, so `video.bit_depth: "12"` combined with `video.encoder: "nvenc"`
fails config validation outright rather than silently falling back, whether
`bit_depth` reached `12` through the key directly or through a `video.variant`
that resolves to a 12-bit row (currently only offered for AV1; no supported
codec offers a 12-bit path on NVENC at all). 12-bit requires the software
encoder tier.

### Linux `platform.*` keys

| Key | Type | Default | What it does | Takes effect |
| --- | --- | --- | --- | --- |
| `platform.auth.mode` | string | `pam` | Only `pam` is accepted in release builds. `none` is refused by SEC-001. | Restart |
| `platform.auth.pam_service` | string | `login` | PAM service name. | Restart |
| `platform.auth.unsafe_allow_remote_no_auth` | bool | `false` | Retired SEC-001 compatibility key. Parses, but release builds refuse startup when `true`. | Restart |
| `platform.capture.monitor` | u32 | `1` | **Linux 1-based** monitor index. Pier subtracts 1 for capenc. Not interchangeable with Windows `output_index`. | Restart |
| `platform.capture.display` | string | `:0`, package `:0` | X display for capture. | Restart |
| `platform.capture.xauthority` | path string | unset | Xauthority for capture child. | Restart |
| `platform.capture.width` | u32 | `0` | Advertised width placeholder when live size not known. | Restart |
| `platform.capture.height` | u32 | `0` | Advertised height placeholder when live size not known. | Restart |
| `platform.session.desktop` | string | `gnome` | `gnome` or `gnome-classic`. | Restart |
| `platform.session.display` | string | built-in `:10`, package `:11` | Dedicated PAM display. Valid `:1` through `:99`. | Restart |
| `platform.session.gpu_head` | string | built-in `DFP-1`, package `DFP-2` | Dedicated NVIDIA head. Valid `DFP-0` through `DFP-3`. | Restart |
| `platform.session.xorg_bin` | path string | built-in `/usr/libexec/Xorg` | Xorg executable. | Restart |
| `platform.session.xorg_config_template` | path string | built-in `/run/arcen/xorg.conf`, package `/etc/arcen/xorg.conf` | Single-head NVIDIA Xorg template. | Restart |
| `platform.session.runtime_root` | path string | `/run/arcen/sessions` | Root-owned session artifact directory. | Restart |
| `platform.session.agent_bin` | path string | discovered or package path | Session agent helper. **Target state:** becomes `arcen-pier session-agent`. | Restart |
| `platform.session.launcher_bin` | path string | discovered or package path | Privileged PAM launcher. **Target state:** becomes `arcen-pier session-launcher`. | Restart |
| `platform.session.zoneinfo_root` | path string | `/usr/share/zoneinfo` | Trusted IANA time-zone database root. | Restart |
| `platform.session.disconnected_idle_timeout_secs` | u64 or null | unset/null | Linux-only opt-in hygiene. When set to a value greater than 0, a persistent desktop that has been continuously disconnected for longer than this limit is torn down before the next connection creates a fresh desktop. Omit or set `null` to preserve the default persistent-forever behaviour. | Restart |
| `platform.multi_monitor.advertise_enabled` | bool | `false` | Advertise and admit fixed-topology multi-monitor sessions. | Restart |
| `platform.multi_monitor.heads` | array of strings | `[]` (full `DFP-0`..`DFP-3` GPU capacity) | Optional ordered **allowlist ceiling** of NVIDIA heads this host may provision, for example `["DFP-0", "DFP-1"]`. Not a count of currently-lit displays: a headless GPU has none, and a session provisions the first N heads of the roster for an N-monitor request. Naming heads only narrows the ceiling. | Restart |
| `platform.multi_monitor.nvenc_session_limit` | u8 or null | unset | Optional operator ceiling for simultaneous NVENC sessions. Unset uses measured runtime admission: Pier opens the planned encoder set rather than guessing from the GPU model. | Restart |
| `platform.multi_monitor.allow_software_fallback` | bool | `false` | Permit exact overflow monitors to use OpenH264/H.264/4:2:0 when the NVENC ceiling is exhausted. Full-color displays receive NVENC priority and the complete roster still fails atomically if any software geometry is unsupported. | Restart |
| `platform.input.mode` | string | built-in `none`, package `uinput` | `none` or `uinput`; `uinput` requires PAM. | Restart |
| `platform.audio.capture_binary` | path string | package path | audiocap helper. **Target state:** becomes `arcen-pier audiocap`. | Restart |
| `platform.audio.user` | string | `session` | `session` or `host` audio runtime owner. | Restart |
| `platform.audio.pactl_binary` | path string | `/usr/bin/pactl` | PulseAudio/PipeWire-Pulse control binary. | Restart |
| `platform.logging.managed_log` | path string | package `/var/log/arcen/arcen-pier.log` | Fixed JSONL file reopened on SIGHUP. | SIGHUP or restart |
| `platform.deskside.enabled` | bool | `false` | Physical-console privacy guard. | Restart |
| `platform.deskside.firmware_sha256` | string | empty | Pinned normalized DMI/chassis hash. | Restart |
| `platform.deskside.console_uid` | u32 | unset | Expected local seat0 owner UID. | Restart |
| `platform.deskside.console_display` | string | unset | Physical console `DISPLAY`. | Restart |
| `platform.deskside.console_xauthority` | path string | unset | Physical console Xauthority. | Restart |
| `platform.deskside.input_devices` | array of paths | `[]` | Expected keyboard/pointer devices. | Restart |
| `platform.deskside.outputs[].name` | string | required per output | Physical output name. | Restart |
| `platform.deskside.outputs[].drm_sha256` | string | required per output | Pinned DRM identity hash. | Restart |
| `platform.deskside.outputs[].edid_sha256` | string | required per output | Pinned EDID hash. | Restart |

### Windows `platform.*` keys

| Key | Type | Default | What it does | Takes effect |
| --- | --- | --- | --- | --- |
| `platform.desktop.adapter` | string | unset | Exact case-insensitive DXGI adapter description. Mutually exclusive with `platform.desktop.output_index`. | Restart |
| `platform.desktop.output` | u32 | `0` when adapter is set | Adapter-local output ordinal. Valid only with `platform.desktop.adapter`. | Restart |
| `platform.desktop.output_index` | u32 | `0` | **Windows 0-based** global attached-output index. Not interchangeable with Linux `monitor`. | Restart |
| `platform.desktop.deskside.enabled` | bool | `false` | Physical-workstation privacy guard. | Restart |
| `platform.desktop.deskside.firmware_sha256` | string | empty | Pinned SMBIOS firmware hash. | Restart |
| `platform.desktop.deskside.capture_sha256` | string | empty | Pinned capture output hash. | Restart |
| `platform.desktop.deskside.monitors[].identity_sha256` | string | required per monitor | Physical monitor identity hash. | Restart |
| `platform.desktop.deskside.monitors[].edid_sha256` | string | required per monitor | Monitor EDID hash. | Restart |
| `platform.iddcx.enabled` | bool | `false` | Dormant source-research gate. Supported packages do not ship an Arcen IddCx driver; leave `false`. | Restart |
| `platform.iddcx.render_adapter.stable_id` | string | unset | Research-only exact adapter selector; has no supported packaged provider. | Restart |
| `platform.iddcx.render_adapter.description` | string | unset | Research-only exact adapter selector; has no supported packaged provider. | Restart |
| `platform.multi_monitor.advertise_enabled` | bool | `false` | Advertise and admit fixed-topology multi-monitor sessions through display paths Windows already enumerates. | Restart |
| `platform.multi_monitor.allowed_adapters` | array of strings | `[]` | Exact case-insensitive DXGI adapter descriptions that multi-monitor may consume. Empty inherits `platform.desktop.adapter`; Deck never selects a GPU. | Restart |
| `platform.multi_monitor.max_monitors` | u8 or null | unset | Optional operator ceiling on the advertised monitor count, 1 through 4. Native NVIDIA headless mode automatically clamps this to its hardware-validated safe provider maximum (currently 2); an explicit lower value still wins. Ordinary attached-output mode cannot probe the interactive desktop from session 0, so set a truthful ceiling there when fewer than four outputs are available. | Restart |
| `platform.multi_monitor.nvenc_session_limit` | u8 or null | unset | Optional operator ceiling for simultaneous NVENC sessions across allowed adapters. Unset uses measured runtime admission: Pier attempts the complete planned encoder set and observes whether it meets the configured QoS thresholds. | Restart |
| `platform.multi_monitor.allow_software_fallback` | bool | `false` | Allow eligible non-4:4:4 monitors to use MF/H.264/YUV420 when exact geometry is supported; otherwise reject the complete roster. | Restart |
| `platform.multi_monitor.nvidia_headless_enabled` | bool | `false` | Provision missing monitors through unused native NVIDIA display IDs on the single allowed display/stream adapter. Requires exactly one `allowed_adapters` entry and cannot be combined with `platform.iddcx.enabled`. | Restart |
| `platform.logging.rotate_mb` | u64 | `32` | Active log rotation size in MiB. | Windows control 201 or restart |
| `platform.first_login_timeout_secs` | u64 | `300` | CP first-login session wait. Valid 30 through 1800. | Restart |

Do not enable `platform.iddcx` on a supported installation. No Arcen IddCx
binary is shipped or installed; the existing unsigned source/build material is
research evidence only. If an administrator independently installs a signed
virtual-display driver, configure Pier's ordinary physical/multi-monitor
inventory against the Windows display paths that driver exposes rather than
enabling this research gate.

#### Reserving a GPU on a multi-GPU host

`allowed_adapters` is what decides which GPU captures and encodes. Capture and
encode are bound together — a monitor is encoded on the adapter that owns its
output — so listing a second adapter is what permits a session to consume it,
whatever `platform.desktop.adapter` says. On a host where one card is reserved
for other work, name only the streaming card:

```json
"multi_monitor": {
  "advertise_enabled": true,
  "allowed_adapters": ["NVIDIA GRID V100D-16Q"],
  "nvidia_headless_enabled": true
}
```

Pier warns at startup when `allowed_adapters` permits an adapter other than
`platform.desktop.adapter`, naming the borrowed GPU. It is a warning and not a
rejection, because genuinely multi-GPU streaming hosts are supported; the point
is that the choice is visible rather than silent.

Pinning to one adapter does not cost monitors. `nvidia_headless_enabled`
provisions the remaining monitors from that adapter's own unused NVIDIA display
IDs — a GRID vGPU typically exposes four output slots with only one connected —
so a single card can still serve a multi-monitor session. It requires exactly
one `allowed_adapters` entry.

An administrator is not expected to know an older GPU's NVENC session count.
Leave `nvenc_session_limit` unset for automatic measured admission: Pier opens
the complete planned encoder set and accepts it only when every region meets
the QoS thresholds. Set the field only when policy/licensing requires a lower
hard ceiling. With `allow_software_fallback: true`, eligible non-4:4:4 monitors
can then move to OpenH264 after same-GPU hardware candidates are exhausted.

At Level 2/3, every Windows connection logs one
`effective Windows multi-monitor admission policy` record containing configured
and effective monitor ceilings, `runtime_probe` versus
`operator_ceiling_then_runtime_probe`, the fallback policy, and allowed
adapters. Measured candidate results are logged separately as aggregate and
per-region encoder-admission measurements.

#### Display scale

Match My Layout defaults to the client's presentable logical workspace. A Mac
display measured as 6016x3384 backing pixels over 3008x1692 logical points is
therefore applied as a 3008x1692 Windows output at 100%, regardless of whether
the panel is Apple, Philips, Dell, or another brand. Applying 200-250% to that
point-sized surface would scale the same HiDPI choice twice.

The explicit per-display HiDPI option instead requests backing pixels and the
matching measured host scale (6016x3384 at 200% in that example), preserving
the same logical workspace with additional pixel density.

Pier carries the requested scale in its synthetic EDID, then applies and
verifies the exact Windows source DPI after topology activation. Windows can
still reject an unsupported step or cap a scale that would make the logical
desktop unusably small. Pier logs requested and effective scale for every
monitor and fails the topology transaction if an explicitly applied scale
cannot be verified.

### Validation errors operators will see

| Error text | Cause |
| --- | --- |
| `Pier config does not exist: <path>` | Required `--config` path missing. |
| `parse Pier config <path>: ...` | Strict JSON parse or unknown key failure. |
| `Pier config tls.mode supports only "pem"` | Unsupported TLS source. |
| `Pier config tls.minimum_version must be TLS1.3` | Bad TLS floor. |
| `Pier config tls.disabled_cipher_suites contains ...` | Bad cipher suite. |
| `Pier config TLS posture leaves no usable cipher suite` | Disabled every suite usable at the version floor. |
| `Pier config tls.expiry_warning_days must be between 0 and 3650` | Bad expiry warning window. |
| `--tls-expected-san may be supplied at most 64 times` | Too many expected SANs. |
| `--tls-expected-san must be a bounded DNS name or IP address without whitespace` | Bad Linux expected SAN. |
| `Pier config tls.expected_sans must contain at most 64 bounded DNS/IP names` | Bad Windows expected SAN list. |
| `logging.level and legacy logging.verbosity are mutually exclusive` | Both logging forms in one file. |
| `legacy logging.verbosity is outside 0..=3` | Bad legacy verbosity. |
| `Pier config auth.reconnect_window_secs: ...` | Shared reconnect window rejected. |
| `clipboard direction must be both, client_to_host, host_to_client, or disabled` | Bad clipboard direction. |
| `clipboard content must be all, text, or image` | Bad clipboard content. |
| `clipboard max_bytes must be from 1 MiB through 20 MiB` or `--clipboard-max-bytes must be from 1 MiB through 20 MiB` | Clipboard cap out of range. |
| `refusing to disable authentication: this build has no unauthenticated mode...` | SEC-001 release build was asked for no-auth by flag or config. |
| `invalid auth mode: <value> (expected pam)` | Linux auth mode not `pam` in release build. |
| `PAM service must contain only ASCII letters, digits, '-' or '_'` | Bad Linux PAM service. |
| `--input-mode uinput requires --auth-mode pam` | Linux input backend needs PAM. |
| `--session-display must be :1 through :99` | Bad Linux dedicated display. |
| `--session-gpu-head must be DFP-0, DFP-1, DFP-2, or DFP-3` | Bad Linux GPU head. |
| `invalid desktop session: <value> (expected gnome|gnome-classic)` | Bad Linux desktop. |
| `invalid input mode: <value> (expected none|uinput)` | Bad Linux input mode. |
| `invalid audio user: <value> (expected session|host)` | Bad Linux audio user. |
| `software-h264 requires --codec h264 --chroma yuv420` | Linux software H.264 incompatible codec/chroma. |
| `OpenH264 software encoding requires h264 + yuv420` | Software backend incompatible codec/chroma. |
| `yuv444 requires --codec h265 (clients HW-decode 4:4:4 only via HEVC)` | Linux H.264 plus YUV444. |
| `yuv444 requires h265 on the macOS VideoToolbox path` | Windows H.264 plus YUV444. |
| `Pier config video.bit_depth: unsupported bit depth ... (expected/want 8\|10\|12)` | Bad `bit_depth` value. |
| `Pier config video.color_range: unsupported colour range ... (expected/want limited\|full)` | Bad `color_range` value. |
| `Pier config video.color_matrix: unsupported colour matrix ... (expected/want identity\|bt709\|bt601\|bt2020ncl)` | Bad `color_matrix` value. |
| `Pier config video.color_policy: unsupported colour policy ... (expected/want always-on\|always-off\|default-on\|default-off)` | Bad `color_policy` value. |
| `Pier config video.variant: unsupported variant ...` | Unknown or incoherent `variant` id (also rejects a syntactically valid id the probe matrix does not offer, for example a 12-bit HEVC row). |
| `Pier config video.variant: variant ... selects a codec/chroma this host cannot run: ...` | The variant id is a real, coherent probe-matrix row, but names a format this host does not run (for example AV1 4:4:4, or 4:2:2). |
| `video.bit_depth 12 cannot work with --encoder nvenc: NVENC has no 12-bit mode at any subsampling; use the software tier for 12-bit` | 12-bit requested (directly or via `variant`) while `encoder` is pinned to `nvenc`. |
| `direct QUIC requires a nonzero UDP port` | Port zero. |
| `direct QUIC requires --tls-cert and --tls-key` or `--tls-cert and --tls-key are required` | Missing TLS material. |
| `Pier config desktop.adapter and desktop.output_index are mutually exclusive` | Windows adapter and global index mixed. |
| `Pier config desktop.adapter must not be empty` | Empty adapter string. |
| `Pier config desktop.output is valid only with desktop.adapter` | Windows output with global index. |
| `Pier config desktop.output requires desktop.adapter` | Windows adapter-local output without adapter. |
| `platform.multi_monitor.nvidia_headless_enabled requires exactly one allowed display/stream adapter` | Native NVIDIA headless provisioning was enabled without one unambiguous streaming GPU. |
| `NVIDIA headless provisioning and platform.iddcx.enabled are mutually exclusive` | Two different dynamic-output providers were enabled at once. |
| `--first-login-timeout-secs must be between 30 and 1800` | Bad Windows first-login timeout. |

## 4. Running and managing the service

### Linux systemd

```sh
sudo systemctl start arcen-pier
sudo systemctl stop arcen-pier
sudo systemctl restart arcen-pier
sudo systemctl reload arcen-pier
sudo systemctl enable arcen-pier
sudo systemctl disable arcen-pier
systemctl is-active arcen-pier
systemctl status arcen-pier --no-pager
journalctl -u arcen-pier -n 200 --no-pager
```

The authoritative JSONL log is `/var/log/arcen/arcen-pier.log`. The logrotate rule rotates daily, at 32 MiB max, keeps up to 100 rotations, compresses old files, creates `0640 root root`, and sends SIGHUP. SIGHUP reopens the managed log, re-resolves logging profile and QoS thresholds, and attempts TLS reload.

Set log level in `/etc/arcen/pier.json`:

```json
{"logging":{"level":2}}
```

Then run:

```sh
sudo systemctl reload arcen-pier
```

`ARCEN_LOG` is still accepted as a fine-grained EnvFilter override. The systemd unit does not set it by default.

### Windows SCM

```powershell
Start-Service ArcenPier
Stop-Service ArcenPier
Restart-Service ArcenPier
Get-Service ArcenPier
sc.exe query ArcenPier
```

Runtime controls:

```powershell
sc.exe control ArcenPier 200  # temporary Level 3 Debug
sc.exe control ArcenPier 201  # reload configured profile and QoS
sc.exe control ArcenPier 202  # reload TLS PEM only
```

Primary logs are `%ProgramData%\Arcen\logs\arcen-pier.log` and `%ProgramData%\Arcen\logs\sessions\arcen-session-agent-<sid>.log`. Windows rotates recognized broker and session logs under `logs\archive`, with `platform.logging.rotate_mb` defaulting to 32 MiB and `logging.retention_days` defaulting to 30 normalized days. If registered, lifecycle mirrors also appear in the Application log:

```powershell
Get-WinEvent -LogName Application -FilterXPath "*[System[Provider[@Name='ArcenPier']]]" |
  Select-Object -First 20 TimeCreated, Id, LevelDisplayName, Message
```

### Reading the logs

Current builds write the broker and session logs as JSONL — one JSON object per
line. Hosts still running an older Pier write a plain tracing line instead
(`2026-08-21T14:10:44.000140Z  INFO arcen::cppipe: ...`), so the commands below
fall back to printing the raw line rather than failing.

```powershell
# Recent activity, raw.
Get-Content 'C:\ProgramData\Arcen\logs\arcen-pier.log' -Tail 30

# Follow live while reproducing a problem.
Get-Content 'C:\ProgramData\Arcen\logs\arcen-pier.log' -Wait -Tail 5

# Readable. `-ErrorAction Stop` is required: ConvertFrom-Json raises a
# NON-TERMINATING error, so without it `catch` never runs and a log line that
# is not JSON floods the console with red instead of printing.
Get-Content 'C:\ProgramData\Arcen\logs\arcen-pier.log' -Tail 30 | ForEach-Object {
  try { $j = $_ | ConvertFrom-Json -ErrorAction Stop
        '{0} {1,-6} {2}' -f $j.timestamp.Substring(11,8), $j.severity, $j.message }
  catch { $_ } }

# Failures only.
Select-String -Path 'C:\ProgramData\Arcen\logs\arcen-pier.log' -Pattern '"severity":"(error|warn)"' |
  Select-Object -Last 20 -ExpandProperty Line

# The newest per-session log. Check this when a session drops but the service
# is healthy: the broker reports only that the connection reset, while the
# session agent names the cause.
Get-ChildItem 'C:\ProgramData\Arcen\logs\sessions' | Sort-Object LastWriteTime -Descending |
  Select-Object -First 1 | Get-Content -Tail 30

# Everything, collected for support.
& "$env:ProgramFiles\Arcen\Pier\arcen-pier.exe" support-bundle --out $env:USERPROFILE\Desktop
```

**If the service will not start, the log may not contain the reason.** Startup
can fail before logging is established, so the newest line in the file can
predate the failed attempt entirely and belong to a previous build. All the
operator sees is `STOPPED` and a service control manager code. Run the Pier in
the foreground instead; it prints the error the service swallows:

```powershell
& "$env:ProgramFiles\Arcen\Pier\arcen-pier.exe" --config 'C:\ProgramData\Arcen\pier.json'
```

That is what identified a configuration written before a later schema field was
added, which reported only `1066/4` from the service control manager.

## 5. TLS

Pier uses an operator-managed PEM for direct QUIC. It never issues, fetches, renews, or rotates certificates at runtime. QUIC always uses TLS 1.3. The shared rustls/ring cipher list is the nine suites listed in the configuration section. Disabling every usable suite fails startup.

Certificate requirements: current validity, not a CA leaf, DNS or IP SAN present, `digitalSignature` key usage when key usage is present, `serverAuth` EKU when EKU is present, matching private key, RSA 2048-bit or stronger, P-256, P-384, or Ed25519. SAN-less CN-only leaves are refused.

Linux permissions: key `0600` root-owned, certificate `0644` or stricter. The current package paths are `/etc/arcen/host.crt` and `/etc/arcen/host.key`.

Windows permissions: `%ProgramData%\Arcen\tls\host.key` must have a protected DACL granting full control only to SYSTEM and Administrators. The Windows TLS loader checks this before loading and refuses weak ACLs. Windows certificate store and CNG keys are unsupported.

Rotate by staging both PEM files and reloading:

```sh
sudo systemctl reload arcen-pier
```

```powershell
sc.exe control ArcenPier 202
```

A failed reload keeps the last good certificate while it remains valid. Once the active certificate expires, new TLS handshakes are refused. Existing QUIC sessions keep the already negotiated key. There is no plaintext or WSS fallback. On Deck, a certificate trust or name mismatch appears as a connection failure or trust prompt depending on the macOS trust path and Deck security mode.

## 6. Observability

All Pier and Deck canonical logs are JSON Lines schema version 1. Important top-level keys are `schema_version`, `timestamp`, `sequence`, `profile_level`, `profile_name`, `severity`, `role`, `component`, `platform`, `target`, optional `event_id`, optional `event_name`, optional `category`, optional `outcome`, `sid`, `user`, `host`, `peer_addr`, `health_state`, `message`, and `fields`.

Operational profiles are cumulative: 0 Critical, 1 Error, 2 Info, 3 Debug. `HEALTH_SNAPSHOT` event 1806 is emitted at Level 0 every 60 seconds as proof of life.

Canonical targets from `shared/telemetry/src/names.rs` are `arcen::auth`, `arcen::display`, `arcen::hid`, `arcen::health`, `arcen::media`, `arcen::net`, `arcen::session`, and `arcen::telemetry`. Additional verified product targets include `arcen::tls`, `arcen::capenc`, `arcen::eventlog`, `arcen::cppipe`, `arcen::ui`, and `arcen::audio` in existing monitoring docs and host modules.

Key lifecycle event IDs:

| ID | Name | Meaning |
| --- | --- | --- |
| 1000 | `SERVICE_START` | Pier process running. |
| 1001 | `SERVICE_STOP` | Clean stop. |
| 1002 | `SERVICE_FAILED` | Startup or runtime failure. |
| 1100 | `SESSION_AUTH_OK` | OS authentication and identity binding succeeded. |
| 1101 | `SESSION_AUTH_FAIL` | Authentication failed. |
| 1102 | `SESSION_STREAM_START` | Media/input streaming active. |
| 1103 | `SESSION_END` | Clean session end. |
| 1104 | `SESSION_INTERRUPTED` | Transport or component failure. |
| 1200-1204 | Display events | Display arm, restore, degraded restore, failed restore, watchdog restore. |
| 1300-1301 | CP events | Windows Credential Provider logon outcome. |
| 1400-1404 | TLS events | Active, expiring, reloaded, reload failed, expired. |
| 1805 | `EFFECTIVE_PROFILE` | Active log profile selected or changed. |
| 1806 | `HEALTH_SNAPSHOT` | 60-second proof of life. |

Healthy successful login trace, simplified:

```json
{"event_id":1000,"event_name":"SERVICE_START","target":"arcen::health","fields":{"component":"pier"}}
{"event_id":1400,"event_name":"TLS_CERTIFICATE_ACTIVE","target":"arcen::tls","fields":{"source":"pem"}}
{"event_id":1100,"event_name":"SESSION_AUTH_OK","target":"arcen::auth","fields":{"auth_method":"password","identity_binding":"pam_or_sid"}}
{"event_id":1200,"event_name":"DISPLAY_ARMED","target":"arcen::display","fields":{"display_backend":"...","policy":"...","changed":true}}
{"event_id":1102,"event_name":"SESSION_STREAM_START","target":"arcen::session","fields":{"encoder":"native-nvenc","codec":"h264","chroma":"yuv420","width":1920,"height":1080}}
```

Interpretation: service started, TLS material loaded, OS authentication succeeded, display ownership was armed if needed, and the encoder produced a media plan. Compare `sid` across records to follow one session.

## 7. Troubleshooting

| Symptom | Check | Likely cause and action |
| --- | --- | --- |
| Pier will not start, TLS error | Linux `journalctl -u arcen-pier`; Windows service log | Missing PEM, key/cert mismatch, SAN-less cert, unsupported key, expired cert, or Windows key DACL too broad. Validate and reload or restart. |
| Pier will not start, port error | Startup stderr or service log reports a QUIC bind failure | Another process owns the configured UDP port, name resolution failed, or QUIC TLS setup failed. Repair UDP 18444/configuration and restart. |
| Pier will not start, config parse error | `validate-config --config <path>` | Unknown JSON key, missing required `audio` or `microphone_input`, or validation error from the table above. |
| Client connects but authentication fails | Event 1101 `SESSION_AUTH_FAIL`; Linux PAM logs; Windows CP or LogonUser stage | Bad account/password, PAM service problem, ambiguous Windows session topology, or CP not registered/ready. Do not enable no-auth. |
| Login takes about 25 seconds | Windows first-login logs | PERF-365: current Windows first login includes a fixed 15 second post-login desktop-stability wait after WTS first reports the exact unlocked session. This prevents black WGC capture before Explorer/DWM stabilizes. It is expected current behavior. |
| Resize causes black bars and host does not follow client window | ServerHello has `supports_display_update:false`; session-agent or broker display logs include `retarget custom timing enumeration unavailable; persistent save disabled` | SEC-364: on some GRID vGPU hosts `NvAPI_DISP_EnumCustomDisplay` is unavailable. Display restore backend is not retarget-capable, so Pier advertises no display update and Deck correctly does not request resize. This is a GPU capability limit, not a config fix. |
| Health reports `overall_state: unavailable` during a working session | Event 1806 `HEALTH_SNAPSHOT` | ERR-366: do not trust `overall_state` yet. Use `SESSION_STREAM_START`, frame/QoS fields, client behavior, and component logs. |
| Capture or encode fails | capenc READY line and event 1102 fields | Performance mode ranks AV1 → HEVC → H.264 on the approved NVENC adapter, then either host uses source-built OpenH264. Exact codec/variant pins reject substitution. Colour Fidelity targets HEVC 4:4:4 instead of silently becoming AV1. `not_built` means the selected backend was not compiled. `capenc READY protocol error: missing cursor` means an old Pier is deployed. Rebuild/deploy the fused Pier. |
| Audio absent but video works | Logs for WASAPI or audiocap | Windows needs a default render endpoint for loopback. Linux audiocap can wait during idle PulseAudio monitor suspension and report a capture gap rather than killing the helper. |
| Deck reports Match My Layout unsupported | Host `AuthRequest` has no `multi_monitor_v1`; Windows `diagnose-host` **in an interactive session** | The pre-auth offer is withheld until `platform.multi_monitor.advertise_enabled` is set. If it is set and Deck still refuses, compare Deck's display count against the host's attached-output count: a host advertises `platform.multi_monitor.max_monitors` (default 4) but can only serve one client monitor per attached capture-capable output, and Deck never silently serves a subset. Set `max_monitors` to the host's real output count so the refusal is immediate and honest. |
| Clipboard absent | ServerHello and session logs | Clipboard starts only after exact clipboard v1 negotiation and authenticated dedicated/user session. No-auth/shared-display sessions advertise disabled. |

## 8. Upgrade and rollback

### Linux

Back up before upgrade:

```sh
sudo cp /etc/arcen/pier.json /etc/arcen/pier.json.before-upgrade
sudo cp -a /etc/arcen/host.crt /etc/arcen/host.crt.before-upgrade
sudo cp -a /etc/arcen/host.key /etc/arcen/host.key.before-upgrade
```

Upgrade with the current deployment script or replace binaries only while stopped. Deploy Pier and helpers together. Old capenc binaries can fail the READY protocol.

```sh
sudo systemctl stop arcen-pier
# install new files
sudo arcen-pier validate-config --config /etc/arcen/pier.json
sudo systemctl start arcen-pier
```

Rollback by stopping the service, restoring the previous binary set and config, validating, and starting. Preserve TLS material, logs, and support bundles unless decommissioning.

### Windows

Back up `%ProgramData%\Arcen\pier.json`, `%ProgramData%\Arcen\tls`, and any display/timezone recovery journal before replacing binaries. Stop the service first:

```powershell
Stop-Service ArcenPier
```

Credential Provider DLL replacement may require unregistering and rebooting because LogonUI can keep the DLL loaded. Use the owned CP uninstall script for the installed mode. Do not edit unrelated Credential Provider registrations.

Rollback service and firewall:

```powershell
Stop-Service ArcenPier -ErrorAction SilentlyContinue
sc.exe delete ArcenPier
Remove-NetFirewallRule -DisplayName 'Arcen Pier QUIC 18444' -ErrorAction SilentlyContinue
```

Preserve `%ProgramData%\Arcen\tls` and logs unless the host is being decommissioned.

## 9. Security posture for admins

| Area | Current posture |
| --- | --- |
| Network | Direct QUIC UDP 18444 inbound to Pier. No Span, mTLS, OIDC, or MFA in current product scope. Restrict it to trusted client networks. Legacy TCP 18443 must remain closed. |
| Linux privilege | `arcen-pier.service` runs as root for PAM/logind/session launch, Xorg ownership, uinput, and helper supervision. User desktop work moves into the authenticated user session. |
| Windows privilege | `ArcenPier` runs as LocalSystem for service control, `LogonUserW`, Credential Provider coordination, display mutation, recovery, and user-session process creation. |
| Credential Provider | Additive provider for remote first login and unlock. It must be signed for production. It does not disable Microsoft's password provider. Autologon must remain disabled. |
| Session agent | Runs in the authenticated user session. It handles clipboard APIs and per-session capture/control IPC. LocalSystem brokers TLS/auth and relays bounded frames. |
| TLS keys | Operator-managed PEM. Windows refuses private keys without restrictive DACL. Linux requires root-owned restrictive files. Self-signed certificates issued by either installer are valid for 825 days; renew with `--force` (Windows) or `new-host-cert.sh --renew` (Linux). |
| Password attempts | **Arcen applies no lockout, backoff, or attempt limit of its own.** Each connection presents one credential to PAM or `LogonUserW`, and throttling is entirely whatever the operating system already enforces. On Linux configure `pam_faillock`; on Windows configure the account-lockout policy. A Pier reachable from a network you do not control, on a host with a permissive PAM stack, is an unthrottled password oracle against a real OS account. |
| Logs and support bundles | Sensitive operational data. Bundles pseudonymize selected identities but are not anonymous. Protect them in storage and transit. |
| Known gaps | PERF-365 first-login delay; SEC-364 arbitrary live resize still depends on a retarget-capable display backend; Windows headless multi-monitor requires display paths supplied by hardware, the hypervisor, or a separately installed signed virtual-display driver; native Linux Wayland remains deferred; ERR-366 `overall_state` unreliable. |

## Proving a host actually streams

Service state and a healthy log together still do
not prove a host delivers video. To prove that end to end, use the session
probe:

```sh
PROBE_HOST=<host-ip> PROBE_USER=<account> PROBE_PASS="$PASSWORD" \
  PROBE_SECONDS=30 python3 tools/session-probe/arcen-session-probe.py
```

It completes the full handshake and reports frame count, bytes and time to
first frame as JSON, exiting non-zero if no frame arrived. A healthy Linux host
with hardware encode looks like this:

```json
{"auth_ok": true, "binary_frames": 64, "binary_bytes": 1014750,
 "first_frame_ms": 1899, "errors": [],
 "server_hello": {"codec": "h265", "encoder_backend": "native-nvenc",
                  "screen_width": 1800, "screen_height": 1168,
                  "supports_display_update": true}}
```

Check `encoder_backend` rather than assuming: `native-nvenc` is the hardware
path, and a fallback to software encode will show up here rather than silently.

A low frame count against a long collection window is not by itself a fault —
an idle desktop legitimately produces few frames. Drive some screen motion
before reading anything into the rate.

## Sessions, RDP and account switching (Windows)

Windows shows one interactive desktop on the physical display at a time, and
the capture pipeline can only duplicate that display's adapter. Everything
below follows from those two facts.

### What the Pier does with an existing session

When an account authenticates, the Pier looks at every WTS session on the
machine and picks one of four outcomes:

| what it finds | what it does |
| --- | --- |
| the console is already this account's, unlocked | attaches to it |
| the console is already this account's, locked | unlocks it through the credential provider |
| the console is at the sign-in screen and the account has no session | signs it in through the credential provider |
| the console is at the sign-in screen and the account has a session **elsewhere** | **moves that session onto the console**, then attaches |

The last row is the case an administrator meets first, because installing the
Pier usually means signing in over RDP. Closing an RDP client does not end the
session: Windows parks it as *disconnected* and puts the physical console back
at the sign-in screen, in a different session. The account's desktop is still
there, just not on the display Arcen can capture.

The Pier moves it, using `WTSConnectSessionW` — the same operation as
`tscon <id> /dest:console`. The session id does not change; only the station it
is displayed on. Applications keep running and nothing is signed out.

Two things make this safe rather than a free-for-all:

- **It never displaces anyone.** The console must be at the sign-in screen with
  no user on it. An account signed in *and attached* somewhere is left alone.
- **It only ever moves a session the account owns**, proved by matching the
  session's user token to the authenticated account before the move.

A session that is merely *disconnected* is treated as holding no display,
whoever it belongs to, which is why a machine two people share keeps working
after both have signed in once. Windows takes the same view — RDP will sign a
second user in over a disconnected session.

A session that cannot be attributed to an account always blocks, in any state.

### After a move, expect a lock screen

Windows locks a session when it is disconnected, so a moved session usually
arrives at the console **locked** and is unlocked through the Arcen Credential
Provider. That needs the provider to be installed and the machine to have
rebooted since. If the log says `no Arcen Credential Provider became ready at
the console`, the move worked and the unlock did not — check the provider, not
the session logic.

### Reading the log when a sign-in is refused

Every bind decision records the whole topology, so the shape of the machine
does not have to be reconstructed afterwards:

```
moving the account's existing session onto the physical console
  source_session=1 console_session=2
  topology=console=2 glass=2 sessions=[0:disconnected/locked/proto0
  1:disconnected/user/locked/proto0 2:connected/locked/proto0 ...]
```

- `console` is `WTSGetActiveConsoleSessionId()`.
- `glass` is the `GlassSessionId` registry value. Microsoft documents this as
  the correct way to find the physical display when RemoteFX/vGPU is in use,
  because the ordinary checks report a remote session as local. On an NVIDIA
  vGPU host the two disagreeing is worth investigating.
- each session reads `id:state[/user][/locked|/unlocked]/protoN`.

`protoN` is `WTSClientProtocolType`. Do not read it as "this session is
remote": it describes the client currently attached, so a disconnected RDP
session reports `proto0` exactly like the console does. Whether a session is on
the physical display is decided by its id matching `console`, and nothing else.

### How long a disconnected session holds the display

The host keeps display authority for `auth.reconnect_window_secs` after a
client drops, so the same client can resume without signing in again. Until it
expires, another user is told the display is owned. The default is 180 seconds;
raise it if unreliable networks matter more than fast switching, lower it to 0
to disable resume entirely.

## Display modes

Deck has three Settings → Displays choices. They are client preferences, not
separate host products. Match My Layout negotiates one independently tagged
region stream per display (up to four) when the host can prove the complete
requested topology. Admission is atomic: a host either serves the whole layout
or fails with an actionable reason; it never silently falls back to one display.

| Deck mode | What Deck asks for | Linux + NVIDIA display guard | Windows + NVAPI-capable NVIDIA | Windows without the NVAPI retarget path |
| --- | --- | --- | --- | --- |
| Match my layout | Every active Mac display, up to four, with fixed topology for the attachment. | Available when `platform.multi_monitor.advertise_enabled` is enabled with an NVENC encoder and an explicit `platform.multi_monitor.heads` allowlist. The host provisions the first N allowed heads headlessly. | Available from already-active Windows paths, or from unused native NVIDIA display IDs when `nvidia_headless_enabled` is true. NVIDIA provisioning is journalled before the first EDID write, re-probed before planning, and committed or rolled back with the physical output transaction. Fixed-mode non-NVIDIA outputs may use OpenH264 at h264/yuv420; full-color 4:4:4 still requires NVENC. | Older or disabled hosts fail the complete Match My Layout request; display changes require reconnect. |
| Primary display only | The primary client display at its fullscreen presentation size. | Works. The stream does not follow the Deck window. | Works. The stream does not follow the Deck window. | Works. The stream does not follow the Deck window. |
| Windowed | One stream that follows the Deck app window. | Works only when the session holds a display guard and ServerHello reports `supports_display_update:true`. | Works when ServerHello reports `supports_display_update:true` and `device_capabilities.display_resolution.resize.available:true`. | Does not resize the remote desktop. The session starts at the initial primary-display size, then Deck cannot retarget it as the window changes. |

The **fullscreen presentation size** is the client display's logical size minus
the macOS safe-area insets, encoder-aligned. It is not the raw panel size. A
fullscreen window on a notched Mac is laid out below the notch, so a 14"
MacBook Pro reporting a 1800x1169 mode presents 1800x1130. Pinning to the panel
size instead made the Deck aspect-fit the stream into the shorter viewport,
which downscaled the remote desktop by about 3% and added letterbox bars either
side — a visible break of Arcen's 1:1 pixel-accuracy promise. Deck logs the
decision in `macOS display mapped to stream resolution` (`presentable`,
`safe_area_*`), in `pinned session sized to the fullscreen presentation area`,
and in `deck viewport geometry`; those three lines together show whether a
session is 1:1. With HiDPI streaming enabled the pinned request is the
backing-pixel presentation size instead.

Settings → Displays → Notch area lets the user take the whole panel instead of
the safe area. macOS gives a *standard* fullscreen window only the safe area and
offers no public API to change that, so Arcen hides the menu bar and Dock and
places a borderless window over the full screen frame
(`covering the main screen for notch fullscreen` logs the requested and applied
frames). Both settings are 1:1; the choice is whether the notch strip is black
or shows remote pixels that the notch itself partly covers. In notch mode the
menu bar and Dock auto-hide rather than disappear, so the pointer at the top
edge still reveals the menu — otherwise a user in a pinned session would have no
way to reach Connection → Disconnect (`Ctrl`+`Option`+`F12`).

Settings → Displays → Remote UI scale controls how large the remote desktop's
own interface is. Windows derives its recommended scaling from the display's
DPI, and the host takes that from the physical size in the EDID it synthesises,
which comes from the client's reported millimetres. Automatic reports the real
panel, so a Retina client asks for a very large remote UI; a fixed percentage
advertises `pixels * 25.4 / (96 * percent / 100)` millimetres instead. The
percentage is *apparent* size: a HiDPI request carries twice as many pixels per
point, so the Deck doubles the advertised percentage to keep the same physical
result. This changes the remote desktop's layout only — the stream stays 1:1.
This mechanism is Windows-specific: `hosts/linux` consumes only `width_px` and
`height_px` from the client layout and never the millimetres, and a GNOME
session on Xorg exposes only 100% and 200% regardless of DPI because fractional
scaling is Wayland-only. On a Linux host the working lever is HiDPI streaming —
off gives roughly twice the apparent interface size of on. Making the setting
effective there would mean the host applying
`org.gnome.desktop.interface text-scaling-factor` (or `Xft.dpi`) inside the
session; that is unimplemented, belongs to the Linux Host owner, and cannot be
validated from a macOS workstation.

A host that cannot set the requested mode serves what it can, and Deck now says
so inside the spare black area rather than leaving a small desktop unexplained:

```text
Host could not set 1792x1120; it is serving 1280x800
(change-display-settings-ex-temporary-fallback).
Windowed resize is unavailable: live resize requires an exact display lease.
```

That is the SEC-364 / DISPLAY-380 capability limit, not a client bug: without
the NVAPI custom-timing path the host can only pick from the modes the display
driver already offers. The stream is still presented 1:1 — centred rather than
bottom-anchored once the gap is bigger than a notch strip.

**Remote UI scale applies to Windows hosts only.** It works by advertising a
physical size, and Windows derives its recommended scaling from the resulting
DPI. Linux hosts ignore the client's millimetres entirely — `hosts/linux`
consumes only `width_px`/`height_px` — and a GNOME session on Xorg exposes just
100% and 200% regardless of DPI, because fractional scaling is Wayland-only.
On a Linux host the working lever is HiDPI streaming: off gives roughly twice
the apparent interface size of on.

Making the setting effective on Linux would mean the host applying
`org.gnome.desktop.interface text-scaling-factor` (or `Xft.dpi`) inside the
session from the client's requested scale. That is unimplemented and belongs to
the Linux Host owner; it cannot be validated from a macOS workstation.

### Headless and virtual display prerequisites

Arcen does not bundle, install, or maintain a Windows virtual-display driver.
Pier consumes display paths that the operating system already exposes.

- **Linux with NVIDIA:** the dedicated Xorg provider can create headless
  NVIDIA heads through the driver's `ConnectedMonitor` and `MetaModes`
  configuration. The head roster is a capacity allowlist, not an
  already-active display count — see "Linux multi-head capacity" below.
  The selected Quadro/datacenter/vGPU profile
  and its NVIDIA license must permit the requested heads and encoder workload.
- **Windows with native NVIDIA outputs:** on supported Quadro/datacenter/vGPU
  profiles, Pier can turn unused NVIDIA display IDs into monitors by writing a
  bounded Arcen EDID. This is not IddCx and installs no driver. Enable it only
  with one explicit `allowed_adapters` entry; that adapter owns the remote
  displays, capture, and NVENC. A recovery journal and watchdog are armed before
  the first EDID write.

Inspect that capacity from the interactive console before enabling it:

```powershell
& "C:\Program Files\Arcen\Pier\arcen-pier.exe" nvapi-inventory --json
```

The report must show inactive `spare_display_ids` with one-bit `output_id`
values on the selected adapter. An output-mask bit without a display ID is not
provisionable. `targetAvailable=false` before the EDID is expected; the
transaction requires it to become available and active before planning.
- **Windows with physical or hypervisor outputs:** Pier can use attached paths
  reported by CCD/DXGI, including outputs supplied by a hypervisor display
  device and its signed guest driver. `QueryDisplayConfig(QDC_ALL_PATHS)` must
  expose at least as many targets as Deck requests.
- **Windows with an external virtual-display driver:** an administrator may
  separately install a reputable, appropriately signed IddCx/indirect-display
  driver (a common approach in some Sunshine/Moonlight headless setups). Arcen
  can consume the resulting Windows display paths, but it does not distribute,
  endorse, configure, service, or roll back that driver. Driver trust,
  Secure-Boot compatibility, updates, and recovery remain the administrator's
  responsibility.

Microsoft Basic Display Adapter cannot invent another target. It can drive only
the paths supplied by the underlying device/hypervisor. Likewise, RDP's
Microsoft Remote Display Adapter belongs to the RDP session and is not a
general display provider that Arcen can reuse for its console/direct session.

Set `platform.multi_monitor.max_monitors` to the number the host can truthfully
serve. A one-output software host may advertise one monitor and work in Primary
Display Only mode, but it must not claim two-monitor Match My Layout. A native
NVIDIA headless host may advertise up to four only after its selected adapter's
spare display IDs and rollback have been verified.

### Linux multi-head capacity

A headless NVIDIA GPU has no attached displays, so there is no "currently
active head" count for Pier to read. `nvidia-xconfig --query-gpu-info` reporting

```text
Number of Display Devices: 0
```

is the expected, healthy state on such a host, and it says nothing about how
many heads a session can provision. The driver still owns a fixed set of output
heads, which the dedicated Xorg server's `Option "ConnectedMonitor"` synthesizes
as connected. The X log states that capacity directly:

```text
(--) NVIDIA(0): Valid display device(s) on GPU-0 at PCI:6:16:0
(--) NVIDIA(0):     DFP-0
(--) NVIDIA(0):     DFP-1
(--) NVIDIA(0):     DFP-2
(--) NVIDIA(0):     DFP-3
```

Pier therefore treats `platform.multi_monitor.heads` as an explicit
**allowlist ceiling** over that capacity. Empty remains fail-closed; pier-linux.example.internal
uses `["DFP-0","DFP-1","DFP-2","DFP-3"]`. Pier provisions the first N allowed
heads for an N-monitor request. Deck always sends a complete 1..=4 active
layout; the host does not need the heads to exist beforehand. Narrow the list
only to reserve heads for another workload on the same GPU.

Each provisioned head becomes its own RandR output. NVIDIA names RandR outputs
after the connector (`DVI-D-0`..`DVI-D-3`), not after the `DFP-N` token used in
`ConnectedMonitor`/`MetaModes`; the same index identifies the same head, and
`session::randr_verify` matches them by that index. Verified on pier-linux.example.internal
(`GRID V100D-16Q`, driver `570.172.08`) for one, two, three, and four
simultaneous heads, including a real three-display Mac layout:

```text
Screen 0: minimum 8 x 8, current 7024 x 2560, maximum 30720 x 17280
DVI-D-0 connected primary 3024x1964+0+0 ...
DVI-D-1 connected 2560x1440+3024+0 ...
DVI-D-2 connected 1440x2560+5584+0 ...
```

Two driver behaviours make verification mandatory rather than optional, and
`session::randr_verify` fails a session closed on both:

- A per-head mode the driver cannot honour is dropped **silently**. The
  remaining heads are then re-centred inside the `Virtual` framebuffer, so the
  server comes up "successfully" with a different layout than was planned. Pier
  avoids the mode-pool problem entirely by driving every head at
  `nvidia-auto-select` and stating the exact client raster as `ViewPortIn`; no
  EDID override or synthetic EDID blob is used, required, or shipped.
- `ViewPortIn` is the head's extent *in the X screen*, so under a `Rotation=`
  clause it is post-rotation. A layout whose bounding box exceeds the `Virtual`
  size is clamped and re-centred instead of refused.

If a requested head count cannot be applied exactly, setup fails atomically
rather than serving a layout the client did not ask for.

For Windowed complaints, check display resize capability, not encoder capability.
Live resize is gated by the display backend. On Windows, the host publishes the
decision in `ServerHello.device_capabilities.display_resolution.resize`:

```json
{
  "available": false,
  "reason": "display_backend_cannot_retarget",
  "explanation": "display backend cannot retarget because it did not prove NVAPI custom timings with verified rollback capability",
  "mechanism": "none",
  "scope": "none"
}
```

If `available` is `false`, Windowed cannot resize that remote desktop. A host
using Microsoft Basic Display Adapter or another backend without the NVIDIA
NVAPI custom-timing path is expected to land here for arbitrary window sizes.
This is the same operational condition tracked as **SEC-364** for GRID vGPU:
resize is unavailable and the visible symptom is black bars rather than a
refused session. Do not treat `native-nvenc` in `encoder_backend` as proof that
Windowed resize will work; live resize is reported by `supports_display_update`
and the `display_resolution.resize` capability.

The Windows display code sets `retarget_capable` only after an exact display
lease with an `nvidia-nvapi-*` backend and
`nvapi-purge-plus-set-display-config-exact` (`hosts/windows/src/display.rs:941-943`).
The Windows resize capability reports `nvapi_custom_timing` and
`arbitrary_custom_timing` only when that NVAPI retarget path is available
(`hosts/windows/src/session.rs:2585-2590`, `hosts/windows/src/session.rs:2603-2616`).
If a live resize is attempted without that proof, `retarget_exact` fails with
`live display backend did not prove NVAPI retarget and rollback capability`
(`hosts/windows/src/display.rs:625-628`).

## Choosing which GPU encodes (Windows)

On a host with more than one GPU, which card streams is an explicit operator
decision. Capture and NVENC stay on the same allowed adapter; Arcen does not
cross-copy a desktop from one GPU merely to encode it on another.

### 1. List what the host actually has

Run this **in an interactive session on the host** — an RDP or console logon, not
a service or a remote shell. Windows only enumerates display outputs inside a
session with a desktop, so a run from SSH or a scheduled task without
`/IT` reports no outputs and is misleading:

```
"C:\Program Files\Arcen\Pier\arcen-pier.exe" diagnose-host
```

```
[0] Microsoft Basic Render Driver kind=MicrosoftBasic pci=1414:008c vram=0 MiB ... nvenc_candidate=no
  output 0 global=Some(0) device=\\.\DISPLAY1 attached=true primary=true rect=1280x800@0,0

[1] NVIDIA GRID RTX6000-8Q kind=Hardware pci=10de:1e30 vram=9530 MiB ... nvenc_candidate=yes
  output 0 global=Some(1) device=\\.\DISPLAY6 attached=true primary=false rect=1800x1168@1280,0

[2] NVIDIA GRID V100D-16Q kind=Hardware pci=10de:1db6 vram=15270 MiB ... nvenc_candidate=yes
  output 0 global=Some(2) device=\\.\DISPLAY2 attached=true primary=false rect=1800x1168@3080,0

Recommended: NVIDIA GRID RTX6000-8Q output 0 (\\.\DISPLAY6) - same-adapter direct
NVENC candidate; non-primary/lower-tier adapters are preferred
```

What matters per adapter:

| Field | Meaning |
| --- | --- |
| `nvenc_candidate=yes` | This adapter can encode in hardware. `no` means the session uses OpenH264 if software limits permit. |
| `attached=true` | It owns a desktop output. An adapter with `outputs: none` cannot be captured. |
| `vram` | Used to rank candidates: **less VRAM ranks first**, so the recommendation prefers the weaker card. |
| `primary=false` | Non-primary outputs rank ahead of the primary. |

`Recommended:` is a generic display/VRAM ranking, not an application compute
policy. Override it whenever the workstation role disagrees. On pier-windows.example.internal,
DaVinci Resolve owns the stronger `GRID RTX6000-8Q`; Arcen uses
`GRID V100D-16Q` for headless displays, capture, and NVENC. The DXGI config
name includes the vendor prefix even though NVAPI's diagnostic name does not.

### 2. Name the adapter in the config

`%ProgramData%\Arcen\pier.json`, under `platform.desktop`:

```json
"platform": {
  "desktop": {
    "adapter": "NVIDIA GRID V100D-16Q",
    "output": 0,
    "deskside": { "enabled": false, "monitors": [] }
  },
  "multi_monitor": {
    "advertise_enabled": true,
    "allowed_adapters": ["NVIDIA GRID V100D-16Q"],
    "max_monitors": 4,
    "nvenc_session_limit": 4,
    "allow_software_fallback": true,
    "nvidia_headless_enabled": true
  }
}
```

- `adapter` — the adapter description **exactly as `diagnose-host` prints it**,
  matched case-insensitively. It must be unambiguous; two adapters with the same
  description are rejected rather than guessed between.
- `output` — the output ordinal **on that adapter**, almost always `0`. This is
  not the global index.

`adapter`/`output` and the legacy `output_index` are mutually exclusive; set one
or the other, never both.

### 3. Confirm before trusting it

From SSH, WinRM, session 0, or another non-interactive shell, validate only the
strict JSON/schema contract:

```
"C:\Program Files\Arcen\Pier\arcen-pier.exe" validate-config --schema-only --config C:\ProgramData\Arcen\pier.json
```

That mode deliberately does not claim a usable DXGI output. Hardware-aware
validation requires an interactive desktop and is the command below. A
non-interactive run that reports `available outputs: []` is not evidence that
the configured adapter disappeared.

```
"C:\Program Files\Arcen\Pier\arcen-pier.exe" validate-config --config C:\ProgramData\Arcen\pier.json
```

again **in an interactive session**. It echoes the resolved selection:

```
config=C:\ProgramData\Arcen\pier.json adapter=NVIDIA GRID V100D-16Q
adapter_output=0 global_output=2 device=\\.\DISPLAY2
```

Then `Restart-Service ArcenPier` and connect once. The session log records every
attached output it saw and which one it took.

### Why not just use `output_index`

The shipped default is `"output_index": 0`, a **global ordinal across every
attached output on the machine**. It moves whenever the set of attached displays
changes — a hypervisor console appearing, or another agent plugging a virtual
monitor, is enough to slide index 0 off the GPU and onto an emulated adapter.

That is not cosmetic. Losing the GPU loses NVENC, and losing NVENC downgrades the
display policy from exact to negotiated, so the client's requested resolution is
attempted, rejected and abandoned — every Deck display mode stops working while
the host still streams. Arcen now moves to an attached NVIDIA output rather than
silently accepting an adapter that cannot encode, and warns when it does, but a
named `adapter` is deterministic and is what a production host should use.

## Encoder selection and software fallback

Both hosts select their encoder at runtime from `"encoder": "auto"` and report
what they chose. Check `encoder_backend` rather than assuming; a fallback shows
up there rather than silently.

| Host | Hardware | Software fallback | Automatic? |
| --- | --- | --- | --- |
| Linux | NVENC (`native-nvenc`) | OpenH264 (`openh264-sw-h264`), statically linked | Yes, on typed NVENC unavailability |
| Windows | NVENC (`native-nvenc`) | OpenH264 (`openh264-sw-h264`), source-built | Yes, but only for h264/yuv420 |

Fallback is a runtime condition, not just a missing GPU. `SessionLimit` covers
NVENC refusing a session because it is already serving its maximum concurrent
streams, so a host with a working NVIDIA card can still fall back under load.

**The software contract is narrower than hardware.** OpenH264 is H.264
Baseline, 4:2:0, at most 1920x1200 at 30 fps. A Linux Pier configured for
H.265 4:4:4 at 60 fps on a larger display will therefore be degraded, not
refused, and will log exactly what it changed:

```json
"software encode selected and the plan was degraded to fit it",
 {"backend":"openh264-sw-h264",
  "requested":"h265 yuv444 1800x1168@60",
  "served":"h264 yuv420 1800x1168@30",
  "codec_changed":true,"chroma_changed":true,
  "fps_clamped":true,"geometry_clamped":false}
```

Deck shows the same truth: `h264 - 1800x1168 - Fallback media path:
openh264-sw-h264`. If a user reports that a session looks worse than usual,
this line is the first thing to read.

Geometry is fitted with the aspect ratio preserved rather than each axis being
clipped, and the session display is set to the fitted size, because the display
is mutated exactly once per session and there is no second modeset.

To force software encode for testing, set `"encoder": "software-h264"` in
`video`. Leave `codec` and `chroma` alone; they will be degraded automatically
and the host will warn at startup naming what it will serve.

No NVIDIA GPU encodes 12-bit at any chroma subsampling, so `video.bit_depth:
"12"` requires the software encoder tier and is rejected at config-validation
time if `video.encoder` is pinned to `nvenc`. See "Colour fidelity policy"
above for the full `video.bit_depth`/`video.color_range`/`video.color_matrix`/
`video.color_policy` picture.

### Third-party notices

The Linux installer writes `/usr/share/doc/arcen/THIRD_PARTY_NOTICES.md`. The
Pier statically incorporates the Cisco OpenH264 source, and BSD-2-Clause
requires a binary distribution to reproduce that notice. No OpenH264 shared
object is shipped or linked; verify with `ldd` on the installed binary.

## Client distribution

`Arcen Deck.app` is a self-contained bundle: the user copies it and
double-clicks it. There is nothing to install and no runtime to provision.

**Before distributing it to anyone**, build it with
`packaging/macos/build-deck-app.sh --release`, which signs with an external
Apple identity, applies the hardened runtime, and notarises. A development
build is unsigned and macOS Gatekeeper will refuse it on any machine that did
not build it, because a copy that arrives by download or AirDrop carries a
quarantine attribute. Verify a candidate build with:

```sh
spctl -a -t exec -vvv "Arcen Deck.app"    # must exit 0
```

Do this on a machine that did not build the bundle. On the build host the check
passes for the wrong reason. See BUILD-376.

## Validation account on pier-linux.example.internal

A local account `arcen-test` (uid 1001) was created on **pier-linux.example.internal** on
2026-07-26 to validate authenticated sessions. It exists because the supplied
AD account `jc` was rejected by PAM and guessing further risked a domain
lockout; creating a local account was the authorised alternative.

It is a local account, not a domain one, has no sudo rights, and its password
is rotated to a fresh random value before each validation run and never
written to disk or into this repository.

Remove it when validation on that host is finished:

```sh
sudo systemctl stop arcen-pier          # only if a session may be attached
sudo pkill -u arcen-test || true
sudo userdel -r arcen-test              # -r also removes /home/arcen-test
getent passwd arcen-test || echo "removed"
sudo systemctl start arcen-pier
```

Check for a leftover session first, because the Linux Pier has been observed
leaving `Xorg` and the session agent running after disconnect (SEC-367):

```sh
ps -u arcen-test -o pid,cmd
ls /run/arcen/sessions/
```

## Known operational gaps

These are open findings, listed so an administrator is not surprised by them.

- **SEC-364** — on GRID vGPU, client-driven resize is silently unavailable.
  `NvAPI_DISP_EnumCustomDisplay` fails, the host falls back to a topology
  backend, and the capability is reported as unsupported, so the client
  correctly stops asking. The visible symptom is black bars either side of the
  desktop. The logic is right; Deck does not show an operator-facing notice, so
  inspect `supports_display_update` and
  `device_capabilities.display_resolution.resize`.
- **DISPLAY-380, validation remaining** — Linux dynamic NVIDIA heads and
  Windows native-NVIDIA EDID provisioning are implemented. Release closure
  still requires authenticated physical pointer, keyboard, Wacom, mixed-DPI,
  encoder-load, and reconnect evidence across the complete 1–4 display matrix.
- **DISPLAY-381, Windows ongoing** — the default-off native-NVIDIA source can
  activate spare display IDs. A two-output V100D transaction now passes:
  NVAPI activates both extended heads, Windows applies the 4072x1700 geometry,
  two NVENC pipelines start, Deck decodes both streams, and all eight region
  input events complete. Three outputs remain blocked: after all three NVAPI
  heads activate, Windows `SetDisplayConfig` can block indefinitely applying
  the mixed landscape/portrait 6632x2820 geometry. The watchdog journal
  restores after reboot, but the call is not safely interruptible. Pier
  therefore refuses three-or-more headless NVIDIA displays before that call.
  Keep `max_monitors` at 2 or lower when enabling
  `platform.multi_monitor.nvidia_headless_enabled`; three-output support
  remains a release blocker.
- **PERF-365** — session start waits a fixed 15 s stability window even when
  the session is already stable on the first poll, which dominates a roughly
  25 s login.
- **ERR-366** — `HEALTH_SNAPSHOT` reports `overall_state: unavailable`
  throughout an otherwise healthy streaming session, so it cannot currently be
  used as a health signal.
- **SEC-367** — after a clean disconnect, the session launcher, `Xorg` and the
  session agent have been observed still running well beyond the reconnect
  window. Check for orphaned `Xorg` processes when decommissioning a host.
- **BUILD-376** — the Deck bundle is currently distributable only to the
  machine that built it: development builds are unsigned and Gatekeeper
  rejects them elsewhere, and the binary is arm64-only while the declared
  minimum OS admits Intel Macs.
- **BUILD-375, partly open** — on a Linux host with *no* usable NVENC, the
  `auto` path arms the session display at the client's requested geometry
  before the NVENC probe result is known. Clients at or below 1920x1080 fall
  back cleanly; a client asking for more will fail to start a session. Forcing
  `"encoder": "software-h264"` works at any client size. Windows resolves a
  non-NVIDIA output to OpenH264 before display creation, fits the desktop to
  the shared 1920x1200p30 contract, and degrades the codec to h264/yuv420
  visibly.
