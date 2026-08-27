# Arcen Pier for Windows — Manual Installation

This is the focused direct-connection delivery path for x64 Windows 10 1809+/11
Pro or Enterprise and Windows Server 2022+. It installs one LocalSystem
Windows service and one additive Credential Provider. It does not install or
configure Arcen Span, Entra, MFA, or brokered Kerberos.

## Build the artifacts

Use an x64 Visual Studio Developer Command Prompt with Visual Studio Build
Tools, Rust, SDK/WDK 10.0.26100, a Visual Studio DriverKit component, and the
x64/x86 Spectre libraries installed:

```cmd
hosts\windows\build.cmd
```

Sign `arcen-pier.exe` and `install-arcen-pier.exe` with the same approved
publisher certificate before production distribution. The current script
enforces the signature gate on the in-process CP DLL because LogonUI loads it
directly.

The script builds with `x86_64-pc-windows-msvc`, static CRT linkage, locked
dependencies, and the `nvenc,mf` feature set (NVENC hardware path plus the
Media Foundation SW H.264 fallback for non-NVIDIA hosts, e.g. VMware SVGA).
It writes these untracked artifacts to `target\arcen-windows-x64\`:

- `arcen-pier.exe`
- `arcen_credential_provider.dll`
- `arcen-cp-harness.exe`
- `install-arcen-pier.exe`, built only after the exact embedded Pier passes the
  QUIC-only binary verifier
- Credential Provider install/uninstall scripts
- when `ARCEN_SIGNED_MICROPHONE_PACKAGE` is supplied, an exact signed
  `driver\` directory with servicing scripts and an exact
  `driver\payload\` PortCls SYS/INF/CAT set

The independently authored capture-only PortCls/WaveRT source is under
`driver\arcen-microphone\`. Install the Pier service first so its deterministic
service SID exists, stop `ArcenPier`, then run elevated
`driver\install-driver.ps1`; uninstall, upgrade, and rollback have separate
scripts and likewise require the service to be stopped. These scripts require a reviewed
INF covered with its SYS by a Microsoft WHCP/WHQL production catalog, reject
test/attestation-only signatures and extra contents, and verify the installed
INF/version. Select `Arcen Microphone` in Windows sound input settings or in the
recording application's device picker before enabling `microphone_input.enabled`.
Arcen never changes Windows recording defaults or communications roles.
Protected signing and physical acceptance gates remain in
`docs\operations\audio-input.md`.

The single-file installer preserves operator settings while atomically
migrating legacy direct-listener configuration to QUIC/UDP 18444 and TLS 1.3.
Its rollback copy is retained under `%ProgramData%\Arcen\rollback`.

Run the harness before registration:

```powershell
.\arcen-cp-harness.exe (Resolve-Path .\arcen_credential_provider.dll)
```

## Prepare the machine

Run 64-bit PowerShell as Administrator.

### TLS material

Pier accepts direct QUIC on UDP 18444 with a PEM certificate chain and exactly
one matching PKCS#8, PKCS#1, or SEC1 PEM private key. Enterprise deployments
should provision an administrator-managed chain/key pair. SMB installations
may generate a local pair with the ship-with-the-repo helper (64-bit
PowerShell 7+):

```powershell
.\hosts\windows\scripts\new-host-cert.ps1
```

With no arguments the helper generates only when both files are absent. A
complete pair is retained and its pins are printed; a partial pair or
unmarked administrator-provided PEM is never overwritten. It creates an
ECDSA P-256/SHA-256 certificate with server-auth EKU and SANs for the hostname,
FQDN, and non-loopback host IP addresses, valid for 825 days. It writes
`%ProgramData%\Arcen\tls\host.crt`, PKCS#8 `host.key`,
`host.fingerprint.txt`, and `host.spki-sha256.txt` with
SYSTEM/Administrators-only ACLs through a recoverable same-directory
transaction.

Renew a helper-managed certificate without changing its SPKI pin:

```powershell
.\hosts\windows\scripts\new-host-cert.ps1 -Renew
```

Pairs created by the previous Arcen helper have no ownership marker. Adopt
that known helper output once with
`new-host-cert.ps1 -Renew -AdoptLegacyHelperPair`; do not use this switch for
enterprise/custom PEM.

Rekey only as an explicit trust-changing operation, then distribute the new
SPKI pin to every Deck:

```powershell
.\hosts\windows\scripts\new-host-cert.ps1 -ForceNewKey
```

Skip the `Copy-Item host.crt` / `Copy-Item host.key` lines and the following
`Set-ArcenPrivateKeyAcl` call below if the helper was used. Pier itself never
issues or rotates certificates. Windows certificate store and CNG-backed keys
are unsupported; no runtime fallback or fetch occurs.

### Encoder selection

The Pier's `capenc` subprocess mode ships inside the same executable with two
encode backends: NVENC (NVIDIA hardware) and source-built OpenH264.
The software path is not VMware-specific:
it is designed for attached Intel, AMD, VMware, and other D3D11 desktop outputs
when Windows.Graphics.Capture and the inbox H.264 MFT are available. VMware is
live-tested; Intel/AMD software fallback validation is still pending. It uses
CPU conversion + encoding; DirectX support alone does not imply hardware video
encoding. The pier resolves the backend via `video.encoder` in `pier.json` or
`--encoder <auto|nvenc|software-h264>` on the CLI. `auto` (default) probes the NVIDIA
encode API on the adapter owning the selected output and falls back to OpenH264 when
that output is non-NVIDIA or the NVIDIA runtime is absent. A device-bound
NVENC initialization failure is classified before the software retry; use
explicit `"software-h264"` for a deliberate CPU comparison. The OpenH264 backend requires
`"codec": "h264"` and `"chroma": "yuv420"`.
Because the inbox encoder requires complete 16×16 macroblocks, Pier first
aligns the requested Windows desktop down (for example, 1800×1168 becomes
1792×1168). If a fixed-mode driver rejects that custom size, Pier filters the
driver's supported modes to 16-aligned candidates before ranking and applying a
fallback; an otherwise closer mode such as 1680×1050 is never selected for MF.
ServerHello reports the actual applied size, so no desktop pixels are silently
cropped by the encoder.

For an explicit backend comparison on the same NVIDIA workstation, test once
with `"encoder": "nvenc"` and once with `"encoder": "software-h264"`; keep the codec and
chroma at H.264/YUV420 for a like-for-like comparison. Pin the backend on
mixed-GPU systems rather than relying on `auto`.

### Proxmox CPU-only lab profile

A validated no-passthrough Proxmox lab profile uses:

- Display: `SPICE (qxl)`;
- Audio Device: `ich9-intel-hda` with `driver=spice`;
- Windows `video.encoder`: `"software-h264"`, codec `"h264"`, chroma `"yuv420"`, and
  `fps: 30`; and
- the exact interactive DXGI description in `platform.desktop.adapter`
  (`"Microsoft Basic Render Driver"` in the validated guest), never the WMI
  device label.

No shared GPU is required. Windows may still label the QXL-compatible PCI
device (`VEN_1B36`, `DEV_0100`) as `Microsoft Basic Display Adapter` while DXGI
reports `Microsoft Basic Render Driver`. `diagnose-host` must nevertheless show
an attached output, D3D11 feature level 11.0 or newer, and the compiled OpenH264
software encoder. `d3d11_video_device=false` is expected for this CPU path;
successful WGC capture plus canonical OpenH264 READY is the authoritative proof.

For a 1800×1168 Deck request, this fixed-mode guest rejects custom 1792×1168
and selects the closest supported 16-aligned mode, currently 1280×800. The
applied size is reported honestly. Windows mid-session `display_update` remains
disabled, so resizing/fullscreen scales the current texture rather than changing
stream pixels.

Host audio requires a default Windows render endpoint. With no emulated audio
device, WASAPI logs `get default console render endpoint: Element not found`
and retries while video continues. After adding the SPICE-backed ICH9 HDA
device, Windows should expose a `Speakers (High Definition Audio Device)`
endpoint and WASAPI loopback should start at 48 kHz stereo. This does not enable
Arcen microphone input; `microphone_input.enabled` remains an independent,
default-off policy.

```powershell
$sourceCert = (Resolve-Path .\host.crt).Path
$sourceKey = (Resolve-Path .\host.key).Path
$programRoot = Join-Path $env:ProgramFiles 'Arcen'
$pierRoot = Join-Path $env:ProgramFiles 'Arcen\Pier'
$dataRoot = Join-Path $env:ProgramData 'Arcen'
$tlsRoot = Join-Path $dataRoot 'tls'
$logRoot = Join-Path $dataRoot 'logs'
$sessionLogRoot = Join-Path $logRoot 'sessions'
$runtimeRoot = Join-Path $dataRoot 'runtime'
$recoveryRoot = Join-Path $dataRoot 'recovery'
$supportRoot = Join-Path $dataRoot 'Support'
$licenseRoot = Join-Path $dataRoot 'licenses'
$desktopAdapter = 'NVIDIA GRID V100D-16Q' # Replace with the target host's DXGI description.

New-Item -ItemType Directory -Force `
  $programRoot, $pierRoot, $dataRoot, $tlsRoot, $logRoot, $sessionLogRoot, $runtimeRoot, $recoveryRoot, $supportRoot, $licenseRoot | Out-Null

function Set-ArcenTreeAcl {
  param(
    [Parameter(Mandatory)][string]$Path,
    [Parameter(Mandatory)][string[]]$Grants
  )

  $items = @(
    Get-Item -LiteralPath $Path -Force
    Get-ChildItem -LiteralPath $Path -Force -Recurse -ErrorAction Stop
  )
  if ($items | Where-Object {
      $_.Attributes -band [IO.FileAttributes]::ReparsePoint
    }) {
    throw "Refusing a reparse point inside protected Arcen path: $Path"
  }

  # Remove every inherited/explicit DACL entry and replace ownership/rights,
  # rather than layering grants onto a directory an unprivileged user may own.
  & icacls.exe $Path /setowner '*S-1-5-32-544' /T /C | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "icacls owner failed: $Path" }
  & icacls.exe $Path /reset /T /C | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "icacls reset failed: $Path" }
  & icacls.exe $Path /grant:r @Grants /T /C | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "icacls grant failed: $Path" }
  & icacls.exe $Path /inheritance:r /T /C | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "icacls inheritance failed: $Path" }
}

function Set-ArcenPrivateKeyAcl {
  param([Parameter(Mandatory)][string]$Path)

  # The TLS loader requires the key leaf—not only its protected parent—to own
  # an explicit protected DACL with exactly these two full-control ACEs.
  & icacls.exe $Path /setowner '*S-1-5-32-544' /C | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "icacls key owner failed: $Path" }
  & icacls.exe $Path /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' /C | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "icacls key grant failed: $Path" }
  & icacls.exe $Path /inheritance:r /C | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "icacls key inheritance failed: $Path" }
}

Set-ArcenTreeAcl $programRoot @(
  '*S-1-5-18:(OI)(CI)F',
  '*S-1-5-32-544:(OI)(CI)F',
  '*S-1-5-11:(OI)(CI)RX'
)
Set-ArcenTreeAcl $dataRoot @(
  '*S-1-5-18:(OI)(CI)F',
  '*S-1-5-32-544:(OI)(CI)F'
)
Set-ArcenTreeAcl $runtimeRoot @(
  '*S-1-5-18:(OI)(CI)F',
  '*S-1-5-32-544:(OI)(CI)F',
  '*S-1-5-11:(OI)(CI)M'
)
Set-ArcenTreeAcl $recoveryRoot @(
  '*S-1-5-18:(OI)(CI)F',
  '*S-1-5-32-544:(OI)(CI)F'
)
Set-ArcenTreeAcl $sessionLogRoot @(
  '*S-1-5-18:(OI)(CI)F',
  '*S-1-5-32-544:(OI)(CI)F'
)
Set-ArcenTreeAcl $licenseRoot @(
  '*S-1-5-18:(OI)(CI)F',
  '*S-1-5-32-544:(OI)(CI)F'
)

Copy-Item .\arcen-pier.exe -Destination $pierRoot
Copy-Item $sourceCert -Destination (Join-Path $tlsRoot 'host.crt')
Copy-Item $sourceKey -Destination (Join-Path $tlsRoot 'host.key')
Set-ArcenPrivateKeyAcl (Join-Path $tlsRoot 'host.key')

$configPath = Join-Path $dataRoot 'pier.json'
$config = @{
  listen = @{
    host = '0.0.0.0'
    port = 18444
  }
  tls = @{
    cert = (Join-Path $tlsRoot 'host.crt')
    key = (Join-Path $tlsRoot 'host.key')
    minimum_version = 'TLS1.3'
  }
  capture = @{}
  video = @{
    codec = 'h265'
    chroma = 'yuv444'
    fps = 60
    encoder = 'auto'   # 'auto' | 'nvenc' | 'software-h264'
  }
  audio = @{
    enabled = $true
    # false = uncompressed PCM for LAN; true = fixed Opus 128 kbps.
    compressed = $false
  }
  microphone_input = @{
    enabled = $false
  }
  clipboard = @{
    direction = 'both' # both | client_to_host | host_to_client | disabled
    content = 'all'    # all | text | image
    max_bytes = 8388608
  }
  # Optional. Create disclaimers\en_US.txt first; startup and validate-config
  # reject missing, empty, invalid UTF-8, or files larger than 16 KiB.
  auth = @{
    disclaimer = @{
      enabled = $false
      locale = 'en_US'
      directory = 'disclaimers' # relative to pier.json
    }
  }
  redirection = @{
    # Opt-in, system-wide LocalSystem mutation. Keep false unless accepted.
    timezone = $false
  }
  logging = @{
    level = 0 # production Level 0 (Critical)
    retention_days = 30
    qos_targets = @{
      fps_degraded_percent = 90
      fps_critical_percent = 70
      rtt_degraded_ms = 60
      rtt_critical_ms = 150
      drop_degraded_basis_points = 50
      drop_critical_basis_points = 500
      input_degraded_ms = 50
      input_critical_ms = 120
      heartbeat_critical_misses = 3
    }
  }
  platform = @{
    desktop = @{
      adapter = $desktopAdapter
      output = 0
    }
    logging = @{
      rotate_mb = 32
    }
    first_login_timeout_secs = 300
  }
}
$json = $config | ConvertTo-Json -Depth 7
[IO.File]::WriteAllText(
  $configPath,
  $json,
  [Text.UTF8Encoding]::new($false)
)
```

The source PEM certificate and key become:

```text
%ProgramData%\Arcen\tls\host.crt
%ProgramData%\Arcen\tls\host.key
```

Every configured `tls.expected_sans` entry must appear exactly in the leaf
certificate SAN. Adding this setting can therefore be a deliberate rollout
break for existing certificates; validate and replace the certificate before
restarting or reloading. Do not reuse another machine's certificate.

The ACL procedure rejects junctions/symlinks, replaces ownership and the full
DACL recursively, gives authenticated users read/execute only on installed
binaries, keeps service data, TLS material, and the recovery directory
SYSTEM/Administrators-only, and grants Modify only on the separate runtime
directory. The recovery directory must never inherit the runtime directory's
Authenticated Users grant.

`desktop.adapter` is matched case-insensitively against the exact DXGI adapter
description. `desktop.output` is that adapter's local output ordinal. Capture
and NVENC encoding remain on this same GPU for zero-copy. Use
`desktop.output_index` only as a legacy global-index fallback, never together
with `desktop.adapter`.

`clipboard` is strict and host-authoritative. `max_bytes` must be from
1,048,576 through 20,971,520 bytes; startup and `validate-config` reject values
outside that range. Equivalent CLI overrides are
`--clipboard-direction`, `--clipboard-content`, and
`--clipboard-max-bytes`; `--no-clipboard` disables the capability. Clipboard is
enabled by default at both/all/8 MiB, but starts only after a Deck negotiates
exact clipboard protocol v1. The LocalSystem broker never accesses the
clipboard; only the authenticated user-session agent on `winsta0\default` does.

The settings file also supports:

- `listen.host` / `listen.port`
- `tls.cert` / `tls.key`
- `tls.minimum_version` (`"tls1.2"` by default, or `"tls1.3"`)
- `tls.disabled_cipher_suites` (empty by default; names are closed and validated)
- `tls.expiry_warning_days` (default 30)
- `tls.expected_sans` (default empty; each configured DNS/IP SAN is required)
- `capture.binary`
- `video.codec`, `video.chroma`, `video.fps`, `video.encoder`
- `audio.enabled`, `audio.compressed`
- `microphone_input.enabled`
- `redirection.timezone` (default `false`; system-wide while one agent is active)
- `platform.desktop.deskside` (default disabled; strict hash-only physical monitor pins)
- `logging.level` (`0` Critical, `1` Error, `2` Info, `3` Debug; production
  default `0`)
- `logging.qos_targets` (validated shared host/client health thresholds)
- legacy `logging.verbosity` for one release only; it is mutually exclusive
  with `logging.level`
- `platform.logging.rotate_mb` (default 32)
- `logging.retention_days` (default 30, normalized to 7–100)
- `platform.first_login_timeout_secs`

Explicit CLI flags override JSON values. `--output-index`, `--adapter-name`,
and `--adapter-output-index` are available as diagnostic overrides.
`--timezone-redirection` and `--no-timezone-redirection` override the file,
with the last supplied override taking precedence.

Deskside has no CLI override. Obtain the hash-only candidate fields from
`arcen-pier diagnose-host --json`, review every active physical output, and pin
all non-capture monitors:

```json
{
  "platform": {
    "desktop": {
      "adapter": "SESSION CAPTURE ADAPTER",
      "output": 0,
      "deskside": {
        "enabled": true,
        "firmware_sha256": "64-lowercase-hex",
        "capture_sha256": "64-lowercase-hex",
        "monitors": [
          {
            "identity_sha256": "64-lowercase-hex",
            "edid_sha256": "64-lowercase-hex"
          }
        ]
      }
    }
  }
}
```

`diagnose-host --json` reports the normalized firmware hash and each output's
capture/monitor hash candidates. Startup requires CPUID to report no hypervisor,
the pinned positive SMBIOS system/chassis hash to match, one pinned
indirect-wired capture target on a hardware adapter, and every physical monitor
pin. Empty, duplicate, malformed, virtual-only, remote, unpinned, stale, or
capture-overlapping evidence refuses. Do not copy pins between machines.
Deskside requires NVENC `ExactIsolated`; MF/VMware and a physical-only capture
panel refuse. The authenticated user-session agent owns keyboard/mouse hooks. Normal
drain and reconnect expiry restore display before releasing hooks; resumable
detach keeps both armed.

If a crash leaves `display-recovery.json`, keep the recovery directory
SYSTEM/Administrators-only and run the existing LocalSystem
`arcen-pier restore-display` procedure before restarting sessions. Never delete
the journal merely to bypass readiness. If display restore fails, hooks are
still released for local operator recovery. SAS/secure desktop, pen/Wacom,
kernel HID, hot-plug, sleep/resume, and driver-reset behavior are not release
claims until the physical Windows matrix passes.

Journal v4 is required for topology-changing automatic recovery. For legacy
v1-v3 journals, run `arcen-pier migrate-display-journal --journal <path>` from
an elevated administrator process in the exact active local-console session.
The guarded command accepts only explicit mutated `nvapi:null` legacy evidence,
proves every output is unambiguously Windows-native/non-NVIDIA, atomically adds
stable all-path plus selected-output identity, then runs normal exact recovery.
Legacy NVIDIA or unknown EDID ownership is not migratable and the journal is
preserved for manual recovery. The command never force-clears insufficient
evidence.

Time-zone redirection is not a per-user or per-WTS setting. The LocalSystem
service temporarily changes the machine time zone under the one-agent broker
lease and records exact recovery state in
`%ProgramData%\Arcen\recovery\timezone-recovery.json`. This installer-created,
reparse-checked directory is restricted to SYSTEM and Administrators; Pier
validates existing path components again before privileged journal access and
does not configure ACLs itself. Pier always reconciles
that journal at startup, even when the feature is disabled. A conflicting
machine state is retained for operator review and disables only further
redirection. After resolving the external change, run the maintenance command from a
LocalSystem context (not an administrator's user token):

```powershell
& (Join-Path $pierRoot 'arcen-pier.exe') restore-timezone
```

Do not delete a conflicted journal until the original/target state has been
adjudicated. `support-bundle` includes only bounded snapshot fingerprints,
phase, and the target Windows key—not the full journal.

From an interactive console session, validate the file without starting a
listener or changing the display:

```powershell
& (Join-Path $pierRoot 'arcen-pier.exe') validate-config `
  --config (Join-Path $dataRoot 'pier.json')
```

The result reports the configured adapter, adapter-local output, resolved
global output, and Windows display device. Adapter resolution is intentionally
performed in the interactive session because session-0 DXGI enumeration does
not reliably expose desktop-attached outputs.

Inventory all adapters and current outputs without changing the display:

```powershell
& (Join-Path $pierRoot 'arcen-pier.exe') diagnose-host
& (Join-Path $pierRoot 'arcen-pier.exe') diagnose-host --json
```

The report includes DXGI/PCI identity, adapter type, VRAM, D3D11 feature level,
video-device support, attached/primary/available outputs, current and supported
modes, NVENC runtime presence, compiled OpenH264 support, and a reasoned
same-adapter recommendation.

Create a bounded diagnostic bundle without starting the service, loading TLS,
or changing the display:

```powershell
& (Join-Path $pierRoot 'arcen-pier.exe') support-bundle
& (Join-Path $pierRoot 'arcen-pier.exe') support-bundle --out C:\SecureIncident
```

The protected default is `%ProgramData%\Arcen\Support`. If it cannot be used,
the command fails and requires `--out`; it never silently chooses another
directory. Bundles are sensitive operational data and may contain usernames,
domains, correlation IDs, or peer addresses from raw logs. Protect and review
them before sharing. TLS certificate/private-key files and configured paths,
hostnames in filenames/manifests, and customer payloads are excluded;
configuration and native event metadata are redacted. See
`docs/operations/support-bundles.md`.

Display mutation and capture require a true WTS console session. Pier rejects
RDP protocol sessions before mutation so an RDP indirect display cannot replace
the configured output or block restore. On VMware, keep the console viewer's
Autofit/automatic guest resize disabled while streaming; otherwise the viewer
can change the guest resolution underneath WGC. The current Windows host
streams one selected output even when Deck reports multiple client monitors.

Review the resulting ACLs before proceeding:

```powershell
icacls (Join-Path $env:ProgramFiles 'Arcen')
icacls $dataRoot
```

## Register the Credential Provider

Production requires an MSVC DLL signed by the configured Authenticode signer:

```powershell
.\install.ps1 `
  -DllPath .\arcen_credential_provider.dll `
  -BuildTarget x86_64-pc-windows-msvc `
  -ExpectedSignerThumbprint '<40-HEX-THUMBPRINT>'
```

The unsigned path is for an isolated lab only:

```powershell
.\install-test.ps1 `
  -DllPath .\arcen_credential_provider.dll `
  -IUnderstandThisModifiesWinlogon
```

The provider is additive and does not disable Microsoft's password provider.

## Enable service-generated SAS

Preserve the previous value before changing it:

```powershell
$policyPath = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
$previousSas = (Get-ItemProperty $policyPath -Name SoftwareSASGeneration `
  -ErrorAction SilentlyContinue).SoftwareSASGeneration
$previousJournal = [Environment]::GetEnvironmentVariable(
  'ARCEN_DISPLAY_RECOVERY_JOURNAL',
  'Machine'
)
@{
  SoftwareSASGeneration = $previousSas
  DisplayRecoveryJournal = $previousJournal
} |
  ConvertTo-Json |
  Set-Content (Join-Path $dataRoot 'install-state.json') -Encoding Ascii
New-ItemProperty $policyPath -Name SoftwareSASGeneration `
  -PropertyType DWord -Value 1 -Force | Out-Null
[Environment]::SetEnvironmentVariable(
  'ARCEN_DISPLAY_RECOVERY_JOURNAL',
  (Join-Path $dataRoot 'runtime\display-recovery.json'),
  'Machine'
)
```

The journal must sit under `runtime`, not in the data root. It is armed by the
per-session agent running under the signed-in user's unelevated token, and the
data root carries a protected DACL with exactly SYSTEM and Administrators
Pointing this
at the root makes every session that mutates the display fail with
`create display recovery journal "...\display-recovery.tmp-<pid>": Access is
denied`. `runtime` is the directory that grants the agent write.

Value `1` permits Windows services, but not ordinary applications, to generate
the Secure Attention Sequence.

## Register the Event Log source

Register the best-effort `ArcenPier` Windows Event Log source before
starting the service, so the very first `SERVICE_START` lifecycle record has
somewhere to land:

```powershell
..\..\packaging\windows\host\eventlog-source.ps1 -Install
```

This is additive, idempotent, and separately owned from the Credential
Provider registration above — it does not touch the CP's CLSID/provider keys
and the CP install/uninstall lifetime does not affect it. It registers
`TypesSupported` (information/warning/error) and an `ArcenOwned` marker under
`HKLM\SYSTEM\CurrentControlSet\Services\EventLog\Application\ArcenPier`; v1
ships no compiled message DLL, so Event Viewer renders the raw, deterministic
insertion strings Pier writes for each lifecycle record (event ID, name,
category, outcome, severity, correlation ID, and the event's approved
fields). Native delivery is best-effort: registration failure never blocks
service startup, and the existing file logs under
`%ProgramData%\Arcen\logs\` remain the primary record either way.

## Install the LocalSystem service

Remove the old lab scheduled task if it exists:

```powershell
Unregister-ScheduledTask -TaskName ArcenPier -Confirm:$false `
  -ErrorAction SilentlyContinue
```

Create the service with a fully quoted executable path:

```powershell
$pierExe = Join-Path $pierRoot 'arcen-pier.exe'
$binaryPath = "`"$pierExe`" service " +
  "--config `"$configPath`""

New-Service -Name ArcenPier `
  -BinaryPathName $binaryPath `
  -DisplayName 'Arcen Pier' `
  -Description 'Arcen direct remote-workstation host' `
  -StartupType Automatic

sc.exe failure ArcenPier reset= 86400 actions= restart/5000/restart/15000/none/0
```

Allow the direct QUIC port if no equivalent managed firewall rule exists:

```powershell
New-NetFirewallRule -DisplayName 'Arcen Pier QUIC' `
  -Direction Inbound -Action Allow -Protocol UDP -LocalPort 18444
```

Start once to catch configuration errors, then reboot so the CP and service
load through the real cold-boot path:

```powershell
Start-Service ArcenPier
Get-Service ArcenPier
Restart-Computer
```

## Validate after reboot

Before connecting:

```powershell
Get-Service ArcenPier
quser
Get-Process LogonUI, winlogon | Select-Object Name, Id, SessionId
Get-AuthenticodeSignature `
  'C:\Program Files\Arcen\CredentialProvider\arcen_credential_provider.dll'
```

There should be no signed-in user session. Connect with the macOS Deck using a
local `MACHINE\user` or traditional `DOMAIN\user` account. Success requires a
new interactive WTS session, Pier binding that SID-matching session, and a
controllable desktop stream.

Service logs are written to:

```text
%ProgramData%\Arcen\logs\arcen-pier.log
%ProgramData%\Arcen\logs\sessions\arcen-session-agent-<sid>.log
```

If the event source above was registered, best-effort lifecycle records
(service start/stop/failure, machine auth outcomes, session stream
start/end/interruption, display arm/restore, and Credential Provider cold
logon, plus TLS activation/expiry/reload outcomes) additionally land in the
`Application` log under provider `ArcenPier`:

```powershell
Get-WinEvent -LogName Application -FilterXPath "*[System[Provider[@Name='ArcenPier']]]" |
  Select-Object -First 20 TimeCreated, Id, LevelDisplayName, Message
```

The broker and correlation-named session-agent files are canonical JSON Lines.
These are additive and best-effort: a missing or failed registration never
changes an auth/session/display/CP outcome, and the file logs above remain
the primary, complete record.

The Pier runs bounded maintenance on startup and every 24 hours. Broker and
active-session files are archived under `logs\archive`; expired archives and
closed session files are removed according to `logging.retention_days`.
Recognized regular files only are considered, and reparse points are rejected.
The sessions directory remains SYSTEM/Administrators-only; the broker grants
the target account write-only access to its correlation-named active file and
revokes that file grant when the session closes or the file is archived.

Runtime profiles do not require a restart:

```powershell
sc.exe control ArcenPier 200  # temporary Level 3 (Debug)
sc.exe control ArcenPier 201  # re-read profile/QoS and restore configured policy
sc.exe control ArcenPier 202  # securely reload the configured PEM chain/key
```

`ARCEN_LOG` remains the highest-priority fine-grained configured filter.
Control 201 keeps the last good filter when the environment or updated JSON is
invalid. Control 202 validates the replacement outside the resolver lock and
atomically switches only new handshakes; active sessions continue unchanged.
Failure retains the last good certificate while it remains valid. Once it
expires, new TLS handshakes are refused without plaintext fallback. The
private broker-agent control is never accepted from a Deck.

The pier-windows.example.internal development deployment currently applies a Level 2 override.
Keep that override in development deployment configuration only; do not change
the packaged Level 0 `pier.json` default.

For an isolated lab investigation, enable secret-safe CP lifecycle diagnostics
before reboot:

```powershell
New-Item -ItemType File -Force `
  (Join-Path $logRoot 'enable-cp-diagnostics') | Out-Null
```

The CP then writes callback names, request ids, and NTSTATUS values—never
accounts or credentials—to
`%ProgramData%\Arcen\logs\credential-provider.log`. Remove the marker after
testing.

## Upgrade and rollback

Stop the service before replacing executables:

```powershell
Stop-Service ArcenPier
```

The CP DLL can remain locked by LogonUI. Unregister it before replacement and
reboot if Windows retains the old image:

```powershell
.\uninstall.ps1 -TestInstall   # omit -TestInstall for a production install
```

Branches before this PR used the inherited legacy Arcen CLSID. Remove only an
ownership-verified legacy Arcen registration before installing the regenerated
CLSID:

```powershell
.\uninstall.ps1 -LegacyArcenInstall -TestInstall
# Omit -TestInstall for a signed production registration. Reboot if the DLL
# remains loaded, then run the new install script.
```

The migration path verifies the Arcen friendly name, install path, threading
model, and provider key before deletion. It refuses non-Arcen registrations
that happen to use the historical CLSID.

Full rollback:

```powershell
$installState = Get-Content (Join-Path $dataRoot 'install-state.json') |
  ConvertFrom-Json
$previousSas = $installState.SoftwareSASGeneration
$previousJournal = $installState.DisplayRecoveryJournal

Stop-Service ArcenPier -ErrorAction SilentlyContinue
sc.exe delete ArcenPier
Remove-NetFirewallRule -DisplayName 'Arcen Pier QUIC' -ErrorAction SilentlyContinue
$testMarker = Join-Path $env:ProgramFiles `
  'Arcen\CredentialProvider\UNSIGNED-TEST-INSTALL.txt'
if (Test-Path $testMarker) {
  .\uninstall.ps1 -TestInstall
} else {
  .\uninstall.ps1
}

if ($null -eq $previousSas) {
  Remove-ItemProperty $policyPath -Name SoftwareSASGeneration `
    -ErrorAction SilentlyContinue
} else {
  New-ItemProperty $policyPath -Name SoftwareSASGeneration `
    -PropertyType DWord -Value $previousSas -Force | Out-Null
}
[Environment]::SetEnvironmentVariable(
  'ARCEN_DISPLAY_RECOVERY_JOURNAL',
  $previousJournal,
  'Machine'
)
```

Reboot after CP removal. Do not delete or modify other Credential Provider
registrations.

Remove the event source only when uninstalling Pier itself, not when merely
upgrading executables or rolling back the Credential Provider:

```powershell
..\..\packaging\windows\host\eventlog-source.ps1 -Uninstall
```

This is idempotent (a missing registration is not an error) and refuses to
remove a registration it does not own.
