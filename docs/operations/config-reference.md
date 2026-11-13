# Pier configuration reference

Every key a Pier accepts in `pier.json`, with its type, default, meaning, when
a change takes effect, and the release that first accepted it. The file is
strict JSON: an unknown key, a wrong type or an invalid value stops the Pier
at start with a message naming the key (see [Validation errors](#validation-errors)).

- Linux: `/etc/arcen/pier.json`
- Windows: `%ProgramData%\Arcen\pier.json`
- macOS: `/Library/Application Support/Arcen/pier.json`

`audio`, `microphone_input` and `platform` are required; every other section
may be omitted. Paths may be absolute; relative `tls`, `capture` and
disclaimer paths (and Linux `platform` paths) resolve against the directory
that holds `pier.json`. Secrets are never written into the file — TLS keys are
paths. Validate before restarting with `arcen-pier validate-config --config <path>`.
How the sections fit together, the installers' defaults and the audio policy
are explained in [Unified Pier configuration](pier-configuration.md); the
[administration guide](pier-administration.md) explains the policies behind
the video, GPU and session keys.

**Since.** The first Arcen release that accepted the key. `≤0.13` means the key
predates 0.13.0, the first release with tagged history. A Pier older than a
key's release refuses a file that contains it, so add a key only after the
Pier is upgraded, and keep the previous `pier.json` for a rollback.

**Kept in step with the code.** `scripts/pier_config_reference.py` reads the
configuration types (`shared/session/src/pier_config.rs` and each host's
`platform` section) and fails — in `python3 -m unittest
scripts/test_operations_references.py` and in
`scripts/check_shared_contracts.py` — when a key the Pier accepts is missing
here, a row names a key it no longer accepts, or a row lacks a type, default,
release or meaning. `--write` adds a row for each new key with its release
computed from git history; fill in the rest.

## Common keys

| Key | Type | Default | Since | Meaning | Takes effect |
| --- | --- | --- | --- | --- | --- |
| `audio.compressed` | bool | Required. Built-in `false`; packaged templates `true` | ≤0.13 | `false` selects PCM; `true` selects fixed Opus policy. | Restart |
| `audio.enabled` | bool | Required. Built-in `false`; packaged templates `true` | ≤0.13 | Enables host-to-Deck audio. | Restart |
| `audio.local_playback` | string | `muted` | ≤0.13 | Whether the host's own speakers stay audible during a session: `muted` (default) keeps the room silent for the whole session, detached time included, and refuses a session on a host that cannot prove it; `audible` keeps them playing. Windows does not implement muting yet. See [Local playback](pier-configuration.md#local-playback). | Restart |
| `auth.disclaimer.directory` | path string | Linux `/etc/arcen/disclaimers`; Windows `disclaimers` relative to config | ≤0.13 | Directory containing `<locale>.txt`. | Restart |
| `auth.disclaimer.enabled` | bool | `false` | ≤0.13 | Requires disclaimer acknowledgement before OS authentication. | Restart |
| `auth.disclaimer.locale` | string | `en_US` | ≤0.13 | Locale file stem. | Restart |
| `auth.reconnect_window_secs` | u32 | `180` | ≤0.13 | Direct resume window. Valid shared range is 0 through 7200. Zero disables resume. The host keeps its display authority for the whole window, so this is also how long another user waits after somebody disconnects. | Restart |
| `capture.binary` | path string | unset | ≤0.13 | Legacy external-helper path; accepted with a warning and ignored because Pier always spawns its own `capenc` subcommand. | Restart |
| `capture.provider` | string or absent | absent (`auto`) | 0.16.1 | Linux. `auto` chooses from host facts (Xorg installed → X11; otherwise a qualified headless Wayland with DRM; otherwise refuse), `xorg` or `wayland` override it and refuse to start when they cannot be honoured. `wayland` is accepted only by a `wayland-provider` build. See "Which desktop a Linux Pier serves" under Installation. Windows and macOS refuse the key. | Restart |
| `capture.wayland_auto_requalify` | bool | `true` | 0.17.0 | Linux headless Wayland. Whether the Pier re-qualifies the host itself at start when an update made its record stale, or when the record skipped X11 applications for an unsafe `/tmp/.X11-unix` that is sound again (once per boot); `false` only refuses (stale record) or serves without X11 applications, naming the command to run. See "Qualifying a host for headless Wayland" in the [administration guide](pier-administration.md). | Restart |
| `capture.wayland_compositor_prefix` | path string | unset (`/usr`) | 0.17.0 | Linux headless Wayland. The prefix GNOME Shell or Mutter is installed in (`<prefix>/bin/gnome-shell`). A side-by-side install such as `/opt/arcen-gnome/51` is used only by Arcen's sessions. See "Upgrading the GNOME session on EL10" in the [administration guide](pier-administration.md). | Restart and requalify |
| `capture.wayland_qualification_record` | path string | unset (the production record) | 0.17.0 | Linux headless Wayland. The qualification record this Pier reads and `arcen-pier wayland qualify` writes. A development Pier names its own so it never reads or overwrites the installed service's. | Restart |
| `clipboard.content` | string | policy default `all`; packages `all` | ≤0.13 | `all`, `text`, or `image`. | Restart |
| `clipboard.direction` | string | policy default `both`; packages `both` | ≤0.13 | `both`, `client_to_host`, `host_to_client`, or `disabled`. | Restart |
| `clipboard.max_bytes` | usize | policy default and packages `8388608` | ≤0.13 | Encoded clipboard cap. Valid 1 MiB through 20 MiB. | Restart |
| `listen.host` | string | Linux built-in `127.0.0.1`, Linux package `0.0.0.0`; Windows `0.0.0.0` | ≤0.13 | Bind address. | Restart |
| `listen.port` | nonzero u16 | `18444` | ≤0.13 | Mandatory direct QUIC UDP port. Bind failure stops startup. | Restart |
| `listen.quic_port` | u16 or absent | absent | ≤0.13 | Deprecated migration alias for the QUIC UDP port; new configs must omit it. | Restart |
| `logging.level` | integer | `0` | ≤0.13 | Operational profile: 0 Critical, 1 Error, 2 Info, 3 Debug. | Linux SIGHUP, Windows control 201, restart |
| `logging.qos_targets.drop_critical_basis_points` | u16 | `500` | ≤0.13 | Critical drop threshold. 500 is 5%. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.drop_degraded_basis_points` | u16 | `50` | ≤0.13 | Degraded drop threshold. 50 is 0.5%. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.fps_critical_percent` | u8 | `70` | ≤0.13 | Critical FPS percent threshold, of the target or, when the capture proves a still source, of the changed-content rate (`fps_content`). | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.fps_degraded_percent` | u8 | `90` | ≤0.13 | Degraded FPS percent threshold, of the same expected rate. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.heartbeat_critical_misses` | u32 | `3` | ≤0.13 | Missed-heartbeat critical threshold. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.input_critical_ms` | u32 | `120` | ≤0.13 | Critical input latency. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.input_degraded_ms` | u32 | `50` | ≤0.13 | Degraded input latency. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.rtt_critical_ms` | u32 | `150` | ≤0.13 | Critical RTT threshold. | Linux SIGHUP, Windows control 201 |
| `logging.qos_targets.rtt_degraded_ms` | u32 | `60` | ≤0.13 | Degraded RTT threshold. | Linux SIGHUP, Windows control 201 |
| `logging.retention_days` | u16 | `30`, normalized 7 through 100 | ≤0.13 | Log archive retention. | Linux SIGHUP, Windows control 201 |
| `logging.verbosity` | integer | unset | ≤0.13 | Legacy one-release mapping: 0→Error, 1→Info, 2/3→Debug. Mutually exclusive with `logging.level`. | Linux SIGHUP, Windows control 201, restart |
| `microphone_input.enabled` | bool | Required. Built-in `false`; packaged templates `true` | ≤0.13 | Enables optional Deck microphone publication policy. | Restart |
| `redirection.timezone` | bool | Built-in `false`; packaged templates `true` | ≤0.13 | Linux sets `TZ` in authenticated desktop process tree. Windows temporarily changes machine time zone under journal. macOS sets `TZ` in the served GUI launchd session for newly launched apps only. | Restart |
| `tls.cert` | path string | package path | ≤0.13 | PEM certificate chain. Alias `certificate` maps to `cert`. | Restart or TLS reload |
| `tls.disabled_cipher_suites` | array of suite names | `[]` | ≤0.13 | Blacklists the three rustls/ring TLS 1.3 suites. | Restart or TLS reload |
| `tls.expected_sans` | array of strings | `[]` | ≤0.13 | Extra exact DNS/IP SANs that must appear in the leaf. | Restart or TLS reload |
| `tls.expiry_warning_days` | u64 | `30` | ≤0.13 | Warning window before certificate expiry. | Restart or TLS reload |
| `tls.key` | path string | package path | ≤0.13 | PEM private key. Alias `private_key` maps to `key`. | Restart or TLS reload |
| `tls.minimum_version` | `TLS1.3` | `TLS1.3` | ≤0.13 | Shipped QUIC requires TLS 1.3. | Restart or TLS reload |
| `tls.mode` | string | unset or `pem` | ≤0.13 | Only PEM files are supported. | Restart or TLS reload |
| `video.bit_depth` | string | Built-in `8`; packages `10` | ≤0.13 | Ceiling coded component depth: `8`, `10`, or `12`. This is a policy ceiling under `video.color_policy`'s `default-*` values (see [Colour fidelity policy](pier-administration.md#colour-fidelity-policy-videocolor_policy)), and an absolute value under its `always-*` values. Packaged hosts expose the proven 10-bit ceiling without forcing it. `12` requires the software encoder tier: NVENC has no 12-bit mode at any subsampling. | Restart |
| `video.chroma` | string or absent | absent in packages | ≤0.13 | Legacy host default (`yuv420` or `yuv444`). Use `video.variant` when the full format must be pinned exactly. | Restart |
| `video.codec` | string or absent | absent in packages | ≤0.13 | Optional exact administrator codec pin: `h264`, `h265`/`hevc`, or `av1`. Omit for automatic client-capability/hardware selection. | Restart |
| `video.color_matrix` | string | `bt709` | ≤0.13 | Ceiling matrix coefficients used to derive luma/chroma from RGB: `identity`, `bt709`, `bt601`, or `bt2020ncl`. | Restart |
| `video.color_policy` | string | `default-off` | ≤0.13 | `always-on`, `always-off`, `default-on`, or `default-off`. Governs how `bit_depth`/`color_range`/`color_matrix` interact with a negotiating client. See [Colour fidelity policy](pier-administration.md#colour-fidelity-policy-videocolor_policy). | Restart |
| `video.color_range` | string | Built-in `limited`; packages `full` | ≤0.13 | Ceiling coded sample range: `limited` or `full`. Existing hand-written deployments that omit it retain `limited`; packaged hosts expose the full-range ceiling under `default-off`. | Restart |
| `video.desktop_encoding` | string | `sdr` | ≤0.13 | Linux. What the Xorg desktop's code values mean, which Xorg cannot report: `sdr` or `rec2100-pq`. Set `rec2100-pq` only when a colour-managed application writes Rec.2100 PQ into the desktop (for example Flame's HDR UI with graphics monitor `Rec.2100-PQ`); a ten-bit HDR request then stays PQ / BT.2020 instead of resolving to Grading, a Grading session is converted to true BT.709 SDR, and eight-bit sessions (Auto, Speed) are refused with a message naming Grading and HDR. Windows rejects any value but `sdr` because it reads HDR state from the OS. | Restart |
| `video.encoder` | string | `auto` | ≤0.13 | `auto`, `nvenc`, or `software-h264` (`openh264` alias on Windows). `nvenc` and `software-h264` are exact backend pins. | Restart |
| `video.fps` | u32 | Linux built-in `60`, Windows `30` | ≤0.13 | Target FPS, 1 through 240. | Restart |
| `video.qp_map` | string or object | Built-in `off`; packaged templates `{"auto":"on","speed":"on"}` | ≤0.13 | Linux, Windows; accepted/no-op on macOS. Damage-driven NVENC QP maps: `off`, `neutral`, or `on`, either as one string for every served pipeline or an object keyed by `auto`, `speed`, `grading`, `hdr`, `software`, `custom`. Missing object keys default to `off`. Linux eight-bit CUDA and macOS VideoToolbox do not carry maps. See `docs/architecture/qp-maps.md`. | Restart |
| `video.variant` | string or absent | absent | ≤0.13 | Strongest exact administrator pin: a complete probe-matrix variant id such as `hevc-444-10-full-bt709`. It overrides the individual format keys and the Deck's automatic codec choice. | Restart |
| `video.x11_desktop_depth` | string | `auto` | 0.17.0 | Linux (Xorg). The depth a new dedicated Xorg desktop is created at: `auto`, `24`, or `30`. Under `auto`, a desktop is created at 30 only for a ten-bit session (Grading, HDR, or ten-bit custom axes) or a `rec2100-pq` desktop, and at 24 otherwise. The depth is fixed for the desktop's whole life; see [Linux desktop depth](pier-administration.md#linux-desktop-depth). Set `30` on hosts used for grading (Flame, Resolve). Other hosts ignore it. | Restart |
| `wayland.first_login_setup` | string | `run` | 0.17.0 | Linux (headless Wayland desktops); other Piers accept and ignore it. `run`: on a user's first desktop, GNOME's initial-setup assistant (`gnome-initial-setup --existing-user`) and GNOME Shell's welcome tour run exactly as after a GDM login. `skip`: before the desktop starts, Arcen writes GNOME's own completion markers for that user (`~/.config/gnome-initial-setup-done` and `org.gnome.shell welcome-dialog-last-shown-version`), as finishing them would have; nothing else changes and no input is faked. `wayland qualify` always skips for its temporary account. Unknown tokens fail configuration loading. | Restart |
| `wayland.lab_seat_pen` | bool | `false` | 0.17.0 | Lab only. Gives each headless Wayland session a virtual tablet on a private udev seat, for a compositor built with the headless-input lab patch. Not for production: it needs a patched Mutter and grants the session user its own virtual pen's event node. | Restart |
| `wayland.session_mode` | string | `auto` | 0.17.0 | Linux (Wayland desktops); other Piers accept and ignore it. `auto`: a new desktop runs natively on its own logind seat with a real pen when the qualification record proves it and the seat can be built (one per host), otherwise headless with the reason logged. `native_seat`: native or the sign-in is refused. `headless`: never native. See "Native desktops and the pen". Unknown tokens fail configuration loading. | Restart |
| `usb.native_tablet` | string | absent (= off) | 0.19.0 | Linux. `enabled`: the Pier offers Native tablet — a Deck's physical pen tablet attached to the signed-in user's X11 or native-seat Wayland desktop as a virtual USB device through the kernel's USB/IP `vhci-hcd` (ADR 0020, ADR 0021) — and loads `vhci-hcd` when it starts. `disabled` or absent: never offered; a Deck asking for it is told so and uses Tablet support. The installer sets it with `--native-tablet` / `--no-native-tablet`. The 0.18 lab switch `ARCEN_USB_HARD_LAB=1` in the service environment is a deprecated alias for this release, honoured only while the key is absent. The Windows Pier never offers Native tablet (ADR 0017); the macOS Pier accepts and ignores the key and offers Native tablet whenever it is signed with the virtual-HID entitlement (ADR 0022). Unknown tokens fail configuration loading. | Restart |

## Linux `platform.*` keys

Linux Piers only. Paths may be relative to `pier.json`.

| Key | Type | Default | Since | Meaning | Takes effect |
| --- | --- | --- | --- | --- | --- |
| `platform.audio.capture_binary` | path string | package path | ≤0.13 | audiocap helper. **Target state:** becomes `arcen-pier audiocap`. | Restart |
| `platform.audio.pactl_binary` | path string | `/usr/bin/pactl` | ≤0.13 | PulseAudio/PipeWire-Pulse control binary. | Restart |
| `platform.audio.user` | string | `session` | ≤0.13 | `session` or `host` audio runtime owner. | Restart |
| `platform.auth.mode` | string | `pam` | ≤0.13 | Only `pam` is accepted in release builds. `none` is refused by SEC-001. | Restart |
| `platform.auth.pam_service` | string | `login` | ≤0.13 | PAM service name. | Restart |
| `platform.auth.unsafe_allow_remote_no_auth` | bool | `false` | ≤0.13 | Retired SEC-001 compatibility key. Parses, but release builds refuse startup when `true`. | Restart |
| `platform.capture.display` | string | `:0`, package `:0` | ≤0.13 | X display for capture. | Restart |
| `platform.capture.height` | u32 | `0` | ≤0.13 | Advertised height placeholder when live size not known. | Restart |
| `platform.capture.monitor` | u32 | `1` | ≤0.13 | **Linux 1-based** monitor index. Pier subtracts 1 for capenc. Not interchangeable with Windows `output_index`. | Restart |
| `platform.capture.width` | u32 | `0` | ≤0.13 | Advertised width placeholder when live size not known. | Restart |
| `platform.capture.xauthority` | path string | unset | ≤0.13 | Xauthority for capture child. | Restart |
| `platform.deskside.console_display` | string | unset | ≤0.13 | Physical console `DISPLAY`. | Restart |
| `platform.deskside.console_uid` | u32 | unset | ≤0.13 | Expected local seat0 owner UID. | Restart |
| `platform.deskside.console_xauthority` | path string | unset | ≤0.13 | Physical console Xauthority. | Restart |
| `platform.deskside.enabled` | bool | `false` | ≤0.13 | Physical-console privacy guard. | Restart |
| `platform.deskside.firmware_sha256` | string | empty | ≤0.13 | Pinned normalized DMI/chassis hash. | Restart |
| `platform.deskside.input_devices` | array of paths | `[]` | ≤0.13 | Expected keyboard/pointer devices. | Restart |
| `platform.deskside.outputs[].drm_sha256` | string | required per output | ≤0.13 | Pinned DRM identity hash. | Restart |
| `platform.deskside.outputs[].edid_sha256` | string | required per output | ≤0.13 | Pinned EDID hash. | Restart |
| `platform.deskside.outputs[].name` | string | required per output | ≤0.13 | Physical output name. | Restart |
| `platform.input.mode` | string | built-in `none`, package `uinput` | ≤0.13 | `none` or `uinput`; `uinput` requires PAM. | Restart |
| `platform.logging.managed_log` | path string | package `/var/log/arcen/arcen-pier.log` | ≤0.13 | Fixed JSONL file reopened on SIGHUP. | SIGHUP or restart |
| `platform.multi_monitor.advertise_enabled` | bool | `true` | ≤0.13 | Administrator off switch. `true` permits advertisement only after startup has found usable NVIDIA heads; `false` withholds `multi_monitor_v1`. | Restart |
| `platform.multi_monitor.allow_software_fallback` | bool | `false` | ≤0.13 | Permit exact overflow monitors to use OpenH264/H.264/4:2:0 when the NVENC ceiling is exhausted. Full-color displays receive NVENC priority and the complete roster still fails atomically if any software geometry is unsupported. | Restart |
| `platform.multi_monitor.heads` | array of strings | `[]` (discover) | ≤0.13 | Empty means run the NVIDIA Xorg head probe at service start. A non-empty list is an administrator override and ordered ceiling, for example `["DFP-0", "DFP-1"]`; Pier provisions the first N heads of the roster for an N-monitor request. | Restart |
| `platform.multi_monitor.nvenc_session_limit` | u8 or null | unset | ≤0.13 | Optional operator ceiling for simultaneous NVENC sessions. Unset uses measured runtime admission: Pier opens the planned encoder set rather than guessing from the GPU model. | Restart |
| `platform.session.agent_bin` | path string | discovered or package path | ≤0.13 | Session agent helper. **Target state:** becomes `arcen-pier session-agent`. | Restart |
| `platform.session.desktop` | string | `gnome` | ≤0.13 | `gnome` or `gnome-classic`. | Restart |
| `platform.session.disconnected_idle_timeout_secs` | u64 or null | unset/null | ≤0.13 | Linux-only opt-in hygiene. When set to a value greater than 0, a persistent desktop that has been continuously disconnected for longer than this limit is torn down before the next connection creates a fresh desktop. Omit or set `null` to preserve the default persistent-forever behaviour. | Restart |
| `platform.session.display` | string | built-in `:10`, package `:11` | ≤0.13 | Dedicated PAM display. Valid `:1` through `:99`. | Restart |
| `platform.session.gpu_head` | string | built-in `DFP-1`, package `DFP-2` | ≤0.13 | Dedicated NVIDIA head. Valid `DFP-0` through `DFP-3`. | Restart |
| `platform.session.launcher_bin` | path string | discovered or package path | ≤0.13 | Privileged PAM launcher. **Target state:** becomes `arcen-pier session-launcher`. | Restart |
| `platform.session.runtime_root` | path string | `/run/arcen/sessions` | ≤0.13 | Root-owned session artifact directory. | Restart |
| `platform.session.wayland_x11_apps` | `auto`, `on` or `off` | `auto` | 0.17.0 | X11 applications in headless Wayland sessions, through the compositor's Xwayland. `auto` serves them where `/usr/bin/Xwayland` is installed and the qualification record proves it (requalify after installing Xwayland); `on` refuses to start unless both hold; `off` starts every compositor with `--no-x11`. Each session then runs one Xwayland (about 80 MB of memory). Logged at start as `X11 applications in Wayland sessions: …`. See [X11 applications (Xwayland)](../architecture/linux-wayland-provider.md#x11-applications-xwayland). | Restart and requalify |
| `platform.session.xorg_bin` | path string | built-in `/usr/libexec/Xorg` | ≤0.13 | Xorg executable. | Restart |
| `platform.session.xorg_config_template` | path string | built-in `/run/arcen/xorg.conf`, package `/etc/arcen/xorg.conf` | ≤0.13 | Single-head NVIDIA Xorg template. Its `BusID` is checked against the host's NVIDIA GPUs before each X server starts. | Restart |
| `platform.session.zoneinfo_root` | path string | `/usr/share/zoneinfo` | ≤0.13 | Trusted IANA time-zone database root. | Restart |

## Windows `platform.*` keys

Windows Piers only. See also "Reserving a GPU on a multi-GPU host" and "Display scale" in the [administration guide](pier-administration.md).

| Key | Type | Default | Since | Meaning | Takes effect |
| --- | --- | --- | --- | --- | --- |
| `platform.desktop.adapter` | string | unset | ≤0.13 | Exact case-insensitive DXGI adapter description. Mutually exclusive with `platform.desktop.output_index`. | Restart |
| `platform.desktop.deskside.capture_sha256` | string | empty | ≤0.13 | Pinned capture output hash. | Restart |
| `platform.desktop.deskside.enabled` | bool | `false` | ≤0.13 | Physical-workstation privacy guard. | Restart |
| `platform.desktop.deskside.firmware_sha256` | string | empty | ≤0.13 | Pinned SMBIOS firmware hash. | Restart |
| `platform.desktop.deskside.monitors[].edid_sha256` | string | required per monitor | ≤0.13 | Monitor EDID hash. | Restart |
| `platform.desktop.deskside.monitors[].identity_sha256` | string | required per monitor | ≤0.13 | Physical monitor identity hash. | Restart |
| `platform.desktop.output` | u32 | `0` when adapter is set | ≤0.13 | Adapter-local output ordinal. Valid only with `platform.desktop.adapter`. | Restart |
| `platform.desktop.output_index` | u32 | `0` | ≤0.13 | **Windows 0-based** global attached-output index. Not interchangeable with Linux `monitor`. | Restart |
| `platform.first_login_timeout_secs` | u64 | `300` | ≤0.13 | CP first-login session wait. Valid 30 through 1800. | Restart |
| `platform.font_smoothing` | string | `unchanged` | 0.17.0 | `grayscale` replaces ClearType with grey-scale text anti-aliasing for the length of each session and restores the user's setting when the session's agent ends. ClearType's sub-pixel colour fringes show as coloured text edges on a Deck, worst on 4:2:0 Auto/Speed streams. The user's stored preference is never written. A volatile per-session marker (`HKCU\Software\ArcenVolatile`, gone at logoff) records the change, so if the agent is killed, the next agent start in that session restores ClearType whatever the policy is then; logoff also restores it. Affects everything drawn in that Windows session, including a physical console. | Next session |
| `platform.iddcx.enabled` | bool | `false` | ≤0.13 | Dormant source-research gate. Supported packages do not ship an Arcen IddCx driver; leave `false`. | Restart |
| `platform.iddcx.render_adapter.description` | string | unset | ≤0.13 | Research-only exact adapter selector; has no supported packaged provider. | Restart |
| `platform.iddcx.render_adapter.stable_id` | string | unset | ≤0.13 | Research-only exact adapter selector; has no supported packaged provider. | Restart |
| `platform.logging.rotate_mb` | u64 | `32` | ≤0.13 | Active log rotation size in MiB. | Windows control 201 or restart |
| `platform.multi_monitor.advertise_enabled` | bool | `true` | ≤0.13 | Administrator off switch. `true` permits advertisement only after startup chooses an eligible live NVIDIA/NVENC streaming adapter; `false` withholds `multi_monitor_v1`. | Restart |
| `platform.multi_monitor.allow_software_fallback` | bool | `true` | ≤0.13 | Allow eligible non-4:4:4 monitors to use MF/H.264/YUV420 when exact geometry is supported; otherwise reject the complete roster. | Restart |
| `platform.multi_monitor.allowed_adapters` | array of strings | `[]` | ≤0.13 | Exact case-insensitive DXGI adapter descriptions that multi-monitor may use. Empty means any eligible adapter that is not excluded; startup resolves the effective streaming adapter. | Restart |
| `platform.multi_monitor.excluded_adapters` | array of strings | `[]` | ≤0.13 | Exact case-insensitive DXGI adapter descriptions multi-monitor must not use. Prefer this to reserve a GPU for other work. | Restart |
| `platform.multi_monitor.max_monitors` | u8 or null | unset | ≤0.13 | Optional operator ceiling on the advertised monitor count, 1 through 4. Native NVIDIA headless mode clamps this to its hardware-validated safe provider maximum (currently 2); an explicit lower value still wins. Ordinary attached-output mode cannot probe the interactive desktop from session 0, so set a truthful ceiling there when fewer than four outputs are available. | Restart |
| `platform.multi_monitor.nvenc_session_limit` | u8 or null | unset | ≤0.13 | Optional operator ceiling for simultaneous NVENC sessions across allowed adapters. Unset uses measured runtime admission: Pier attempts the complete planned encoder set and observes whether it meets the configured QoS thresholds. | Restart |
| `platform.multi_monitor.nvidia_headless_enabled` | bool or null | unset/null | ≤0.13 | Provision missing monitors through unused native NVIDIA display IDs on the selected streaming adapter. `null` lets startup enable it for a GRID/Quadro-class adapter, `false` is an off switch, and `true` forces the provider. Cannot be combined with `platform.iddcx.enabled`. | Restart |

## macOS `platform.*` keys

macOS Piers only. The macOS Pier is not yet a supported host release.

| Key | Type | Default | Since | Meaning | Takes effect |
| --- | --- | --- | --- | --- | --- |
| `platform.agent_bundle_id` | string | `pier.arcen.tech.agent` | ≤0.13 | Bundle identifier of the per-session agent the Pier launches into the GUI session. Must not be empty. | Restart |
| `platform.input_backend` | string | `auto` | ≤0.13 | How keyboard input reaches a signed-in desktop: `auto` (CGEvent in a signed-in session, virtual HID at the login window), `coregraphics`, or `virtual_hid` (a virtual HID keyboard wherever one can be created). The pointer stays on CGEvent. The login window always uses virtual HID. | Restart |
| `platform.multi_monitor.advertise_enabled` | bool | `false` built in; packaged template `true` | ≤0.13 | Explicit opt-in for Match My Layout. Necessary, not sufficient: the host also needs native multi-display capture, region input and virtual displays before it advertises. | Restart |
| `platform.multi_monitor.max_monitors` | u8 or null | unset | ≤0.13 | Optional operator ceiling on the advertised monitor count. | Restart |
| `platform.native_login_enabled` | bool | `false` | ≤0.13 | Retired; nothing reads it. Still accepted so an existing `pier.json` keeps loading. | — |
| `platform.privileged_broker_enabled` | bool | `false` | ≤0.13 | Retired; nothing reads it. Still accepted so an existing `pier.json` keeps loading. | — |
| `platform.service_user` | string | `_arcen` | ≤0.13 | The unprivileged account the Pier's service side runs as. Must not be empty. | Restart |
| `platform.virtual_display_enabled` | bool | `false` | ≤0.13 | Retired; nothing reads it. Still accepted so an existing `pier.json` keeps loading. | — |
## Accepted values

`tls.disabled_cipher_suites` takes exactly these names:
`TLS13_AES_256_GCM_SHA384`, `TLS13_AES_128_GCM_SHA256`,
`TLS13_CHACHA20_POLY1305_SHA256`, `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`,
`TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`,
`TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`,
`TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`,
`TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`, and
`TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256`.

`logging.qos_targets` must be ordered: FPS degraded at most 100 and above FPS
critical; RTT degraded nonzero and below RTT critical; drop degraded below drop
critical, and drop critical at most 10000; input degraded nonzero and below
input critical; heartbeat critical misses nonzero.

## Validation errors

The messages a Pier prints when it refuses a configuration, and what each
means, are listed under "Validation errors operators will see" in the
[administration guide](pier-administration.md#validation-errors-operators-will-see).
Every refusal a running Pier sends to a Deck, and every code the Deck's
connection test reports, is in [Error codes](error-codes.md).
