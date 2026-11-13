# Error codes

Every failure Arcen reports carries a stable code next to its message: the
connection test's and the colour check's findings, the codes a Pier sends a
Deck when it refuses, the reason classes in the Pier's log, and the numbered
log events. This page
lists every one, what it means and what to do. Search it for the code you
see. Symptoms without a code are in [Troubleshooting](troubleshooting.md).

Where the codes appear:

| Code family | Where you see it |
| --- | --- |
| `diag.*` | The Deck's **Test connection** report and `arcen-deck diagnose <host>` |
| `colour.*` | The Deck's **Colour check** panel and its copied or saved report |
| Session resume | The Deck's reconnect message; the resume result's `error_code` on the wire |
| Linux sign-in refusals | The Deck's sign-in error; `reason_class` of `SESSION_AUTH_FAIL` (1101) and `SECURITY:` lines in the Linux Pier log |
| Disconnected desktop, deskside, native seat | The Deck's sign-in error and the Pier log |
| Grading and HDR refusals | The Deck's sign-in error (a Windows Pier leads it with the code), the Colour check's "last connection ended", and the Pier log |
| Windows HDR session refusals | The Deck's sign-in error (the code leads the message) and the Windows Pier's `session agent failed` line |
| Wayland provider, qualification, installer | `arcen-pier wayland qualify`, the installer, `DESKTOP_PROVIDER_UNAVAILABLE` (2005) and the Pier's start-up log |
| Lane refusals | `reason_class` of `LANE_REFUSED` (2000) in either side's log |
| Numbered events | `event_id` / `event_name` in the JSON Lines log, the journal and the Windows event log; the full schema is in [Monitoring](monitoring.md) |

**Kept complete by a check.** `scripts/error_code_reference.py` reads each
code from the enum that produces it and fails — in `python3 -m unittest
scripts/test_operations_references.py` and `scripts/check_shared_contracts.py`
— when a code is added without a row here, a row names a code that no
longer exists, or a code source has an arm it cannot read (a catch-all, or a
code taken from another enum without a declared, itself-checked delegation). A Pier configuration that will not load reports a message,
not a code; those are listed in the
[administration guide](pier-administration.md#validation-errors-operators-will-see).

## Connection test

Run from the Deck's connection editor (**Test connection**) or the command line
(`"/Applications/Arcen Deck.app/Contents/MacOS/arcen-deck" diagnose <host[:port]>`;
on an Intel Mac, `Arcen Deck Intel.app`).
It checks, in order: name resolution, UDP reachability (a real QUIC handshake),
the certificate against the identity this Deck remembers, the Pier's greeting
before sign-in, the path MTU and the round trip. It never sends a user name or
password. Exit status: 0 ready, 1 warnings, 2 cannot connect.

| Code | Meaning | What to do |
| --- | --- | --- |
| `diag.dns.resolved` | **PASS.** The name resolved. | Nothing. |
| `diag.dns.no_addresses` | **FAIL.** The name exists but has no address. | Check the spelling, or use the Pier's IP address. If the name only resolves inside your organisation, connect to its network or VPN first. |
| `diag.dns.failed` | **FAIL.** The name could not be resolved. | Check the spelling, or use the Pier's IP address. If the name only resolves inside your organisation, connect to its network or VPN first. |
| `diag.dns.timeout` | **FAIL.** The name server did not answer in time. | Check that this Mac is online and its name servers answer, or use the Pier's IP address. |
| `diag.udp.handshake_ok` | **PASS.** The Pier answered on its UDP port and the encrypted handshake completed. | Nothing. |
| `diag.udp.refused` | **FAIL.** The machine answered, but nothing is listening on that UDP port. | The Pier service is not running or listens on another port. On the Pier: Linux `systemctl status arcen-pier`, Windows `Get-Service ArcenPier`, macOS `launchctl print system/com.arcen.pier`; compare `listen.port` in pier.json with the port you used. |
| `diag.udp.local_network_unreachable` | **FAIL.** This Mac has no route to that local-network address. | Allow Arcen Deck in System Settings › Privacy & Security › Local Network, then check that the Pier is switched on and on the same network. |
| `diag.udp.unreachable` | **FAIL.** There is no route to the Pier. | Check that the Pier is switched on, that this Mac's network or VPN is up, and that no router drops the route. |
| `diag.udp.timeout` | **FAIL.** Nothing answered on that UDP port. | The Pier is down, the port is wrong, or a firewall drops UDP to it. Check that the Pier service runs and listens on this port (listen.port, default 18444), then allow inbound UDP on the Pier's firewall and on any firewall or VPN in between. Arcen uses UDP only, never TCP. |
| `diag.udp.transport_mismatch` | **FAIL.** The Pier answered but speaks a different Arcen transport version. | Update the Deck and the Pier to the same Arcen release. |
| `diag.udp.handshake_failed` | **FAIL.** The Pier answered but the handshake failed. | Note the detail line, update the Deck and the Pier to the same release, and check the Pier's log for a TLS or listener error. |
| `diag.cert.remembered` | **PASS.** The Pier presented the identity this Deck remembers. | Nothing. |
| `diag.cert.supplied_match` | **PASS.** The Pier presented the fingerprint you supplied. | Nothing. |
| `diag.cert.system_trusted` | **PASS.** The certificate is issued by an authority this computer trusts. | Nothing. |
| `diag.cert.first_use` | **INFO.** This Deck has not met this Pier before; it will show the fingerprint and ask you to confirm it. | Before you trust it, compare the SPKI fingerprint below with the Pier's: host.spki-sha256 in the Pier's configuration directory, or the spki_sha256 field of its TLS_CERTIFICATE_ACTIVE (1400) log record. |
| `diag.cert.mismatch` | **FAIL.** The Pier presented a different identity from the one this Deck expects. | Do not sign in. Ask the Pier's administrator whether its certificate was replaced and compare the SPKI fingerprint below with host.spki-sha256 on the Pier. Only if they match, choose Forget and Verify in the Deck. |
| `diag.cert.name_mismatch` | **FAIL.** The certificate does not name the address you connected to. | Connect with a name or address the certificate lists, or reissue the certificate with this name (tls.expected_sans, or rerun the installer). |
| `diag.cert.expired` | **FAIL.** The Pier's certificate has expired. | Renew the Pier's certificate (rerun the installer; Linux and macOS also `arcen-pier new-host-cert`), and check that the Pier's and this Mac's clocks are right. |
| `diag.cert.not_yet_valid` | **FAIL.** The Pier's certificate is not valid yet. | Renew the Pier's certificate (rerun the installer; Linux and macOS also `arcen-pier new-host-cert`), and check that the Pier's and this Mac's clocks are right. |
| `diag.cert.expiring` | **WARN.** The Pier's certificate expires soon. | Renew the Pier's certificate before it expires. A renewal that keeps the key keeps the SPKI fingerprint, so Decks are not asked again. |
| `diag.cert.unusable` | **FAIL.** The Pier's certificate cannot be used. | Replace the Pier's certificate with one the installer generates, or one from your authority with an RSA ≥ 2048 or ECDSA key. |
| `diag.greeting.sign_in` | **PASS.** The Pier is ready and asks for sign-in. Diagnostics stop here; no credentials were sent. | Sign in with an account on the Pier. The Pier's version, platform and capabilities appear in the Deck's session details after sign-in. |
| `diag.greeting.no_sign_in` | **WARN.** The Pier served without asking for sign-in, which a release build never does. | This is a laboratory build. Do not use it on a network you do not control. |
| `diag.greeting.unexpected` | **FAIL.** The Pier sent something other than a sign-in request. | Update the Deck and the Pier to the same Arcen release. |
| `diag.greeting.closed` | **FAIL.** The Pier closed the connection before asking for sign-in. | Read the Pier's log at the time of this test for the reason it closed the connection (for example a full host or a refused peer). |
| `diag.greeting.timeout` | **FAIL.** The Pier completed the handshake but did not ask for sign-in. | The Pier may be overloaded or stuck. Check its log and restart the service if it does not answer. |
| `diag.mtu.fits` | **PASS.** Arcen's packets cross this path in both directions. | Nothing. |
| `diag.mtu.too_small` | **FAIL.** The network interface is too small for Arcen's packets. | Raise the MTU of the VPN or tunnel to at least 1280; QUIC cannot run below 1200-byte packets. |
| `diag.mtu.unknown` | **INFO.** The path size could not be checked. | Nothing. |
| `diag.rtt.good` | **PASS.** Round trips are fast enough for interactive work. | Nothing. |
| `diag.rtt.degraded` | **WARN.** Round trips are slow enough to notice. | Prefer a wired connection or a closer network path; choose Speed for a lighter stream. |
| `diag.rtt.high` | **WARN.** Round trips are slow; drawing and typing will lag. | Use a closer or wired network path; a VPN that hairpins through a distant site adds every millisecond here. |
| `diag.rtt.unknown` | **INFO.** The round-trip time could not be measured. | Nothing. |
| `diag.not_run` | **SKIP.** An earlier step failed, so this one did not run. | Nothing. |

## Colour check

The Deck's **Colour check** (Session › Colour Check…, or Settings › Streaming ›
Colour check…) explains what the host serves, what this Mac does with it and
whether the code values arrive exactly; see the
[colour check guide](colour-check.md). **PASS** is right for colour-critical
work, **INFO** is a fact, **WARN** is a caveat and **FAIL** means what you see
is not what the host has. No fix changes a host setting.

| Code | Meaning | What to do |
| --- | --- | --- |
| `colour.host.not_connected` | **INFO.** Not connected: what a host serves can only be checked in a session. | Connect to a host, then open Colour check from the Session menu. |
| `colour.preset.served` | **PASS.** The host serves the preset this Deck asked for. | Nothing. |
| `colour.preset.downgraded` | **FAIL.** The host served a different pipeline from the one this Deck asked for. | Read the host's reason and its prerequisite below. Fix that on the host, or choose a preset this host can serve; the Deck never changes host settings. |
| `colour.preset.eight_bit` | **WARN.** Auto and Speed are eight-bit 4:2:0 streams: right for everyday work, not for judging colour. | Switch to Grading (ten-bit 4:4:4, full range, BT.709) — or HDR on an HDR display — for colour-critical review. |
| `colour.preset.custom` | **WARN.** A developer-defined stream is active instead of one of the four presets. | Choose Grading or HDR to return to a supported, documented contract. |
| `colour.preset.unreported` | **INFO.** This host does not report which pipeline it served; the items below rely on its colour description alone. | Update the Pier to the same Arcen release as this Deck. |
| `colour.codec.profile` | **PASS.** The codec and profile carry the chroma and depth asked for. | Nothing. |
| `colour.codec.changed` | **WARN.** The host chose a different codec from the one asked for. | Nothing, if chroma and depth pass; otherwise read the host's reason. |
| `colour.codec.unknown` | **INFO.** The host did not name its codec. | Nothing. |
| `colour.chroma.full` | **PASS.** Full 4:4:4 chroma: every pixel carries its own colour. | Nothing. |
| `colour.chroma.reduced` | **FAIL.** The host subsampled chroma although 4:4:4 was asked for: fine colour detail and thin coloured lines are averaged. | See the host's reason under Preset and its prerequisite below. |
| `colour.chroma.subsampled` | **WARN.** 4:2:0 chroma: colour is stored once per 2×2 pixels, so coloured text and thin lines fringe. | Switch to Grading (ten-bit 4:4:4, full range, BT.709) — or HDR on an HDR display — for colour-critical review. |
| `colour.chroma.unknown` | **INFO.** The host did not describe its chroma sampling. | Nothing. |
| `colour.depth.ten` | **PASS.** Ten bits per sample: the stream adds no banding. | Nothing. |
| `colour.depth.reduced` | **FAIL.** The host served eight bits although ten were asked for. | See the host's reason under Preset and its prerequisite below. |
| `colour.depth.eight` | **WARN.** Eight bits per sample: smooth gradients can band. | Switch to Grading (ten-bit 4:4:4, full range, BT.709) — or HDR on an HDR display — for colour-critical review. |
| `colour.depth.unknown` | **INFO.** The host did not describe its bit depth. | Nothing. |
| `colour.signal.sdr` | **PASS.** SDR with the transfer and primaries asked for (Grading is SDR by design). | Nothing. |
| `colour.signal.hdr` | **PASS.** HDR as asked for: PQ transfer with BT.2020 primaries. | Nothing. |
| `colour.signal.changed` | **FAIL.** The host's transfer or primaries differ from the request (for example HDR served as SDR). | See the host's reason under Preset and its prerequisite below. |
| `colour.signal.unknown` | **INFO.** The host did not describe its transfer or primaries. | Nothing. |
| `colour.coding.full` | **PASS.** Full-range coding with the matrix asked for: every code value is used. | Nothing. |
| `colour.coding.limited` | **WARN.** Limited (video) range: on the eight-bit scale, codes below 16 and above 235 cannot be told apart. | Switch to Grading (ten-bit 4:4:4, full range, BT.709) — or HDR on an HDR display — for colour-critical review. |
| `colour.coding.changed` | **FAIL.** The host's range or matrix differs from the request. | See the host's reason under Preset and its prerequisite below. |
| `colour.coding.unknown` | **INFO.** The host did not describe its range or matrix. | Nothing. |
| `colour.frames.agree` | **PASS.** The decoded frames' own headers match what the host announced. | Nothing. |
| `colour.frames.disagree` | **FAIL.** Decoded frames describe a different picture from the host's announcement. | Copy this report and send it with the Pier's log: this is a defect. |
| `colour.frames.none` | **INFO.** No frame has been decoded yet. | Nothing. |
| `colour.capture.implied` | **INFO.** The host does not tell the Deck its capture source; this is the source its served pipeline uses on this platform. | Nothing. |
| `colour.encoder.hardware` | **PASS.** Hardware encoder. | Nothing. |
| `colour.encoder.software` | **WARN.** Software encoder: colour follows the contract, but frame rate is limited. | Check the host's GPU and its free encoder sessions; see the Pier's log. |
| `colour.encoder.unknown` | **INFO.** The host did not say whether its encoder is hardware. | Nothing. |
| `colour.display.reference` | **PASS.** macOS reports a reference mode on this display: it renders without tone mapping up to its reference level. | Nothing. |
| `colour.display.not_reference` | **WARN.** macOS reports no reference mode on this display: brightness, True Tone and ambient adaptation change what you see. | For judging colour, use a calibrated display or a reference preset (System Settings › Displays › Preset, for example HDTV Video BT.709-BT.1886 for Grading), and turn off True Tone and automatic brightness. |
| `colour.display.unknown` | **INFO.** The Deck could not read this display's colour state. | Nothing. |
| `colour.display.profile` | **INFO.** macOS converts the Deck's tagged picture into this display's profile: colours match by appearance; the codes sent to the panel equal the stream's only when the profile is the tag. | Nothing. |
| `colour.display.hdr_ready` | **PASS.** This display has HDR headroom for the PQ stream. | Nothing. |
| `colour.display.no_headroom` | **FAIL.** The stream is HDR but this display shows no HDR headroom: highlights clip at SDR white. | Use an HDR display (Liquid Retina XDR, Pro Display XDR, or an external display with HDR turned on in System Settings › Displays), or switch to Grading. |
| `colour.display.sdr` | **INFO.** SDR stream: the display's HDR headroom is not used. | Nothing. |
| `colour.present.ten_bit` | **PASS.** Ten-bit stream on the dedicated ten-bit Metal layer: no eight-bit step on this Mac. | Nothing. |
| `colour.present.fallback` | **FAIL.** A ten-bit stream is shown through the eight-bit fallback. | Reconnect to rebuild the layer. If it persists, copy this report: the reason is below. |
| `colour.present.eight_bit` | **PASS.** Eight-bit stream on the ordinary SDR path, as its contract says. | Nothing. |
| `colour.present.none` | **INFO.** Nothing is being presented yet. | Nothing. |
| `colour.edr.correct` | **PASS.** EDR follows the transfer: on only for PQ. | Nothing. |
| `colour.edr.mismatch` | **FAIL.** EDR does not match the stream's transfer. | Copy this report and reconnect: this is a defect. |
| `colour.edr.unknown` | **INFO.** The presentation layer's EDR state is not known yet. | Nothing. |
| `colour.scale.one_to_one` | **PASS.** One stream pixel lands on one screen pixel. | Nothing. |
| `colour.scale.integer` | **WARN.** The picture is enlarged by a whole number: each stream pixel covers several screen pixels. | Stream at Retina resolution (HiDPI) for this display so one stream pixel is one screen pixel. |
| `colour.scale.resampled` | **FAIL.** The picture is resized to fit: resampling blends neighbouring pixels, so the values on screen are not the stream's. | Enter fullscreen or make the window large enough for 1:1; when already fullscreen, reconnect so the host can match this screen's size. |
| `colour.scale.unknown` | **INFO.** The picture's scale is not known yet. | Nothing. |
| `colour.exact.not_run` | **INFO.** Exactness has not been measured in this session. | Show the Arcen test pattern full-screen at one image pixel per screen pixel on the host (Save pattern writes a PNG at this stream's exact size), then press Measure. |
| `colour.exact.waiting` | **INFO.** Waiting for the next decoded frame to measure. | Nothing. |
| `colour.exact.exact` | **PASS.** Every pixel of the pattern arrived with its exact code values. | Nothing. |
| `colour.exact.near` | **WARN.** Within two codes of the pattern: the lossy codec's quantisation. Close, not bit-exact. | Expected for a lossy stream. For bit-exact still frames, see lossless refinement below. |
| `colour.exact.off` | **FAIL.** Code values differ from the pattern by more than two codes. | Check that the pattern fills the host's screen at 1:1 with no viewer scaling, border or colour management, that it is the pattern selected here, and that the preset is Grading. |
| `colour.exact.bt709_reencoded` | **WARN.** The host re-encoded the desktop's sRGB values with the BT.709 transfer: the code values differ from the desktop's (up to 16/255 in the shadows), and shown as SDR they make dark tones darker than on the host. | Expected from a Windows Pier before 0.18.0, which captures linear light and re-encodes it as BT.709, and from a Linux Pier before 0.19.0 serving a declared Rec.2100 PQ desktop or a Wayland HDR composition as Grading. Current Piers serve this Deck the desktop's own codes (sRGB transfer). |
| `colour.exact.localised` | **WARN.** Within two codes everywhere except a few pixels or one small area: something on the host is drawn over the pattern there (a notification, a watermark, a pointer). | Close or move what covers the area named below, then measure again; the rest of the picture is already measured. |
| `colour.exact.hdr` | **INFO.** HDR maps the SDR desktop to PQ at 203-nit white, so the desktop's code values change by design: there is nothing to compare code for code. | Measure code values in Grading. |
| `colour.exact.unavailable` | **INFO.** This frame could not be read back for measurement. | Measure again; the detail line says why. |
| `colour.exact.refinement` | **INFO.** Bit-exact still frames need lossless refinement (ADR 0016), which is proposed and not in this release: a lossy stream's exactness is measured here, not assumed. | Nothing. |
| `colour.host.ready` | **PASS.** Nothing on the host stands between it and the preset asked for. | Nothing. |
| `colour.host.grading_needs` | **INFO.** What Grading and HDR need on this host. | This is a host setting: ask the host's administrator, using the linked documentation. The Deck never changes it. |
| `colour.host.linux_depth` | **WARN.** This Xorg desktop runs at depth 24, so it cannot serve ten-bit colour. | An Xorg desktop keeps the depth it started with: sign out of it (that closes its applications) and reconnect with Grading, which starts a depth-30 desktop under x11_desktop_depth auto — or ask the host's administrator to set x11_desktop_depth to 30. The Deck never changes it. |
| `colour.host.xorg_hdr` | **INFO.** Xorg cannot report HDR: HDR is served as Grading unless the host's administrator declares a Rec.2100 PQ desktop. | This is a host setting: ask the host's administrator, using the linked documentation. The Deck never changes it. |
| `colour.host.wayland` | **WARN.** This Wayland desktop cannot serve Grading or HDR: GNOME's screen sharing carries ten-bit and HDR only from Mutter 50. | This is a host setting: ask the host's administrator, using the linked documentation. The Deck never changes it. |
| `colour.host.windows_hdr` | **WARN.** Windows serves HDR only when the display Arcen captures is proven to be in HDR mode. | This is a host setting: ask the host's administrator, using the linked documentation. The Deck never changes it. |
| `colour.host.macos_pier` | **INFO.** The macOS Pier is a preview: HDR needs its virtual display to report headroom above SDR white. | This is a host setting: ask the host's administrator, using the linked documentation. The Deck never changes it. |
| `colour.host.reason` | **WARN.** The host gave a reason for serving less than asked for. | This is a host setting: ask the host's administrator, using the linked documentation. The Deck never changes it. |

## Session resume refusals

A Deck that lost its connection presents a one-time resume grant instead of a
password (`auth.reconnect_window_secs`). When the Pier refuses the grant the
Deck asks for the password again; nothing is lost but the shortcut.

| Code | Meaning | What to do |
| --- | --- | --- |
| `unsupported` | The Pier does not offer resume (`auth.reconnect_window_secs` is 0, or an older Pier). | Sign in again. Raise `auth.reconnect_window_secs` if resume is wanted. |
| `expired` | The grant outlived the reconnect window. | Sign in again; the desktop is still there if it has not idled out. |
| `replayed` | The grant was already used once. | Sign in again. Repeated occurrences without a second Deck point at a replayed packet; check the network. |
| `native_identity_changed` | The account or OS session behind the grant changed (signed out, switched user). | Sign in again. |
| `topology_changed` | The Pier's identity or transport changed (new certificate, Pier restarted on another address). | Sign in again; if the certificate changed, verify it first. |
| `session_gone` | The desktop the grant pointed at no longer exists. | Sign in again; a fresh desktop starts. |
| `internal_failure` | The Pier failed while checking the grant and does not say why. | Sign in again, and read the Pier log at that time. |

## Linux sign-in refusals

The Linux Pier's privileged launcher opens the PAM session and starts the
desktop. When it cannot, the Deck shows the message and the Pier logs the
reason class below (`SESSION_AUTH_FAIL`, 1101, and the launcher's own lines in
`/var/log/arcen/arcen-pier.log`). The polkit codes are the launcher refusing a
desktop it could not keep away from the console's administrator rights.

| Code | Meaning | What to do |
| --- | --- | --- |
| `binary_unavailable` | The session launcher is not installed where the Pier expects it. | Rerun the installer, or fix `platform.session.launcher_bin`. |
| `rejected` | The account or password was refused by PAM. | Check the account and password; read the system journal for the PAM service (`platform.auth.pam_service`, default `login`). |
| `protocol_error` | The Pier and its launcher disagreed (often a partially upgraded install). | Rerun the installer so the Pier and launcher are the same build, then restart the service. |
| `timeout` | The launcher did not answer in time. | Check host load and the launcher's log; a hung PAM module (for example an unreachable directory server) shows here. |
| `io_error` | The launcher process failed to run. | Read the Pier log for the operating-system error. |
| `identity_invalid` | The authenticated account cannot have a desktop: root, an unknown user, or a user whose runtime directory or session bus is missing or unsafe. | Use an ordinary account; check `/run/user/<uid>` ownership and that `systemd --user` starts for it. |
| `agent_error` | The per-session agent failed to start or answer. | Read the newest session log under `/run/arcen/sessions/<sid>/`. |
| `pam_init_failed` | PAM could not be initialised. | Check `/etc/pam.d/<pam_service>` exists and is valid. |
| `pam_session_failed` | PAM accepted the password but could not open the session. | Read the journal for the failing PAM module (often `pam_systemd`, `pam_selinux`, home-directory mount). |
| `logind_session_missing` | `pam_systemd` did not create the logind session the desktop needs. | Make sure the PAM service includes `pam_systemd`, and that the Pier runs as the packaged systemd service. |
| `root_required` | The launcher is not running as root. | Reinstall; do not run the Pier by hand as a normal user. |
| `xorg_config_invalid` | The dedicated Xorg template is invalid or names no NVIDIA GPU. | Rerun the installer, or fix `BusID` in `/etc/arcen/xorg.conf` (see the administration guide). |
| `xorg_start_failed` | The dedicated X server did not start. | Read `/run/arcen/sessions/<sid>/Xorg.log`; `(EE) No devices detected` means a wrong `BusID` or no `nvidia` driver. |
| `xorg_verify_failed` | The X server started but its outputs were not the ones asked for. | Read the Xorg log and the head-probe log in `/run/arcen/sessions/discovery/`. |
| `xorg_release_failed` | A previous X server could not be proved gone. | Wait a few seconds and sign in again; if it persists, restart the Pier. |
| `logind_activation_failed` | logind would not make the session active on its seat. | Check `loginctl list-sessions`; another active session on `seat0` blocks it. |
| `seat_occupied` | Another X11 desktop holds `seat0` (one X11 desktop per host). | Sign out of the other desktop, or let it reach its idle timeout. |
| `logind_unlock_failed` | The session could not be unlocked after sign-in. | Read the journal for logind errors; sign in again. |
| `deskside_refused` | Physical-console privacy (`platform.deskside`) refused this desktop. | See [Deskside privacy refusals](#deskside-privacy-refusals). |
| `wayland_unavailable` | This Pier was built without the headless Wayland provider. | Install the Wayland-enabled Pier build, or install Xorg. |
| `wayland_start_failed` | The headless Wayland session did not become ready. | Read the session log; requalify with `arcen-pier wayland qualify --write` after any GNOME or driver update. |
| `local_playback_refused` | The host could not prove its own speakers silent (`audio.local_playback: muted`). | Fix the user's audio server, or set `audio.local_playback` to `audible`. |
| `wayland_release_failed` | A previous Wayland session could not be proved gone. | Sign in again; if it persists, restart the Pier. |
| `selinux_context_refused` | SELinux policy will not let the launcher start the session in the context PAM assigned. | Load the `arcen` module (`semodule -i /usr/share/arcen/selinux/arcen.cil`) or rerun the installer. Never `setenforce 0`. |
| `native_seat_unavailable` | `wayland.session_mode` is `native_seat` and this desktop cannot have its own seat. | See [Native Wayland seat](#native-wayland-seat) for the reason in the log. |
| `polkit_no_rules` | polkit is older than 0.106 and cannot load Arcen's rule. | Upgrade polkit. |
| `polkit_rule_missing` | Arcen's polkit rule is not installed. | `arcen-pier polkit-rule install` (or rerun the installer) and restart the Pier. |
| `polkit_grants_remote_session` | A rule numbered below `05-` grants an administrator action to the remote session. | Narrow that rule; check with `arcen-pier polkit-rule status`. |
| `polkit_unverified` | polkit did not answer (`pkaction`/`pkcheck` failed). | Start polkit (`systemctl status polkit`) and sign in again. |

## Taking over a disconnected desktop

A user who signs in while their own desktop waits disconnected gets it back.
These are the cases where the Pier refuses instead.

| Code | Meaning | What to do |
| --- | --- | --- |
| `another_users_desktop` | The waiting desktop belongs to another user. | Wait until it idles out or that user signs out; the Pier keeps it for `auth.reconnect_window_secs`. |
| `owner_did_not_answer` | The waiting desktop did not answer the hand-over in time. | Sign in again; if it keeps failing, restart the Pier to clear the stuck desktop. |

## Deskside privacy refusals

`platform.deskside` (Linux) and `platform.desktop.deskside` (Windows) refuse to
stream a physical workstation unless the evidence proves the pinned console.

| Code | Meaning | What to do |
| --- | --- | --- |
| `stale_evidence` | The evidence was not collected for this attempt. | Sign in again. |
| `missing_evidence` | A required fact or operator pin is absent. | Fill in every pin in the deskside section (firmware hash, outputs or monitors, input devices). |
| `unknown_evidence` | A required fact could not be classified. | Read the Pier log for the fact; update the pins after a hardware change. |
| `virtual_evidence` | A virtual machine or virtual resource was seen. | Deskside protection is for physical workstations; turn it off on virtual hosts. |
| `remote_evidence` | A remote session or resource was seen. | End the other remote session. |
| `paravirtual_evidence` | A paravirtual device or host was seen. | As for `virtual_evidence`. |
| `conflicting_evidence` | What the host sees contradicts the operator's pins. | Re-pin after an intended hardware change; otherwise investigate the change. |
| `missing_input_fingerprint` | The input pin set has no usable fingerprint. | Set `input_devices` (Linux) to the console's keyboard and pointer. |
| `missing_display_fingerprint` | The display pin set has no usable fingerprint. | Set the outputs (Linux) or monitors (Windows) pins. |

## Native Wayland seat

With `wayland.session_mode` `auto` a desktop falls back to headless and logs
one of these; with `native_seat` the sign-in is refused with it.

| Code | Meaning | What to do |
| --- | --- | --- |
| `configured_headless` | `wayland.session_mode` is `headless`. | Nothing, unless you want a native seat (and a real pen): set `auto`. |
| `not_qualified` | The qualification record has no native-seat proof. | `arcen-pier wayland qualify --write` and restart. |
| `qualification_failed` | Qualification tried a native seat and it failed. | Read the qualification output; fix the reason it names and requalify. |
| `no_gpu_card` | No NVIDIA DRM card node. | Load `nvidia_drm` with `modeset=1`. |
| `gpu_has_connectors` | The GPU has display connectors and may drive the console. | Use a GPU without connectors for native seats, or keep headless. |
| `gpu_in_use` | The GPU's card node is open (an X server or display manager on `seat0`). | Stop the display manager on that GPU, or keep headless. |
| `vkms_missing` | The `vkms` kernel module is not installed. | Install the kernel's extra modules package. |
| `vkms_in_use` | `vkms` is already loaded by another desktop. | One native desktop per host; wait for it to end. |
| `greeter_on_seat` | A display manager's greeter was on the seat first. | Disable the display manager on that seat. |
| `seat_setup_failed` | Building the seat failed (udev, logind or the module). | Read the Pier log for the failing step. |
| `polkit_no_rules` | polkit is older than 0.106, so a desktop on its own seat would keep the console's polkit rights. | Upgrade polkit, or keep `wayland.session_mode` on `headless`. |
| `polkit_rule_missing` | Arcen's polkit rule is not installed, so a native seat cannot be kept from local administrator rights. | `arcen-pier polkit-rule install` and restart the Pier. |
| `polkit_grants_remote_session` | A process in the desktop's session was still granted an action polkit gives only local users. | Narrow the rule numbered below `05-` that grants it; `arcen-pier polkit-rule status`. |
| `polkit_unverified` | polkit did not answer, so the session could not be proven remote. | Start polkit (`systemctl status polkit`) and sign in again. |

## Wayland provider blockers

Why a Linux Pier cannot serve headless Wayland (`DESKTOP_PROVIDER_UNAVAILABLE`,
2005, and `arcen-pier wayland qualify`).

| Code | Meaning | What to do |
| --- | --- | --- |
| `compositor` | No usable headless compositor (GNOME Shell or Mutter with `--headless`). | Install a supported GNOME (see "Upgrading the GNOME session on EL10"). |
| `session_service` | No session service (systemd-logind, D-Bus). | Install and enable them. |
| `pipewire` | No PipeWire screencast. | Install PipeWire and WirePlumber. |
| `render_node_access` | No DRM render node the session can open. | Install the GPU driver; make `/dev/dri/renderD*` openable by session users. |
| `capture_route` | No capture route delivered an encoded frame. | Requalify after fixing the driver; read the qualification output. |
| `input_injection` | No session-bound input injection. | Requalify; check the compositor version against the administration guide. |

## Wayland qualification

The qualification record proves the host's Wayland stack. A record goes stale
when the host changes; with `capture.wayland_auto_requalify` (the default) the
Pier requalifies itself at start.

| Code | Meaning | What to do |
| --- | --- | --- |
| `pier_version` | The record was written by another Pier version. | Requalify (`arcen-pier wayland qualify --write`), or let auto-requalify run. |
| `pier_revision` | Another source revision. | As above. |
| `pier_executable` | Another Pier executable. | As above. |
| `compositor` | Another compositor. | As above. |
| `compositor_version` | Another compositor version (a GNOME update). | As above. |
| `compositor_prefix` | The compositor runs from another prefix. | As above; check `capture.wayland_compositor_prefix`. |
| `pipewire_version` | Another PipeWire version. | As above. |
| `gpu_driver` | The GPU driver or its DRM module changed. | As above, after the driver update finishes. |
| `missing` | No record at the path. | `arcen-pier wayland qualify --write`. |
| `unreadable` | The record could not be read, or is not a root-owned file only root can change. | Make it owned and writable only by root, or requalify. |
| `malformed` | The record is not valid JSON of this schema. | Requalify. |
| `unsupported_schema` | The record was written for another schema. | Requalify. |
| `stale` | The host changed since qualification (one of the reasons above). | Requalify. |

## X11 applications in Wayland sessions

Why a Linux Wayland Pier serves its desktops without X11 applications
(Xwayland). The Pier logs the reason at start (`X11 applications in Wayland
sessions: not served (<code>): <remedy>`), and `arcen-pier wayland qualify`
prints it as `X11 apps at start-up`.

| Code | Meaning | What to do |
| --- | --- | --- |
| `operator_off` | `platform.session.wayland_x11_apps` is `off`. | Set it to `auto` or `on`. |
| `xwayland_not_installed` | No `/usr/bin/Xwayland`. | Install the distribution's Xwayland package, then requalify. |
| `not_qualified` | The record has no Xwayland proof. | Requalify. |
| `qualification_failed` | Qualification tried Xwayland and it failed. | Read the qualification's `x11_apps` failure, fix it, requalify. |
| `x11_unix_dir_foreign_owner` | `/tmp/.X11-unix` is owned by a user, not root (usually: systemd's daily `/tmp` clean removed it on an idle host and that user's compositor recreated it). GNOME Shell aborts at start for every other user if it runs Xwayland. Logged as an error at start, checked before every desktop; qualification skips X11 and records it as a host precondition. | As root, `arcen-pier repair-x11-unix-dir` (runs `systemd-tmpfiles --boot --create --prefix=/tmp/.X11-unix`); then remove the user's stale `X<n>` sockets once none of their X servers runs. If the record skipped X11, the Pier re-proves it at its next start (`--requalify` restarts it now). |
| `x11_unix_dir_not_shared` | `/tmp/.X11-unix` is not `1777` (not sticky or not world-writable). | As above. |
| `x11_unix_dir_not_a_directory` | Something other than a directory (for example a symlink) is at `/tmp/.X11-unix`. | `rm /tmp/.X11-unix` (does not follow a symlink), then `arcen-pier repair-x11-unix-dir`. |
| `x11_unix_dir_missing` | `/tmp/.X11-unix` is gone and could not be recreated (the Pier recreates it itself as root). | `arcen-pier repair-x11-unix-dir`, or reboot. |
| `ice_unix_dir_*` | The same four for `/tmp/.ICE-unix` (X session management). Reported only; desktops keep X11 applications. | The same commands; the repair command fixes both. |
| `x11_precondition_cleared` | Qualification skipped X11 for one of the faults above, which is fixed now; nothing has proved Xwayland yet. With `capture.wayland_auto_requalify` (the default) the Pier re-proves X11 applications itself at start, once per boot (outcomes below). | Restart the Pier (or `arcen-pier repair-x11-unix-dir --requalify`), or requalify (`arcen-pier wayland qualify --write`). |

The automatic re-proof after `x11_precondition_cleared` logs a typed outcome
(`x11_requalify_outcome`) and records it in its attempt marker
(`<record stem>-x11-requalify.json`) and, when it wrote a record, in the
record's `automatic_requalification` section. When it does not run it logs
why (`x11_requalify`).

| Code | Meaning | What to do |
| --- | --- | --- |
| `x11_apps_proven` | Xwayland proven; desktops serve X11 applications. | Nothing. |
| `x11_apps_failed` | Xwayland was tried and failed; only the record's X11 proof is replaced, and it says why (`qualification_failed` from now on). | Read the record's `x11_apps` failure, fix it, requalify. |
| `x11_apps_not_tried` | X11 was not tried (the setting is `off` or Xwayland is not installed now). | As `operator_off` / `xwayland_not_installed`. |
| `precondition_returned` | The socket directory was unsafe again during the run; the record is left unchanged (it keeps the precondition). | Find what changes `/tmp/.X11-unix`, then `arcen-pier repair-x11-unix-dir`. |
| `not_recorded` | The re-qualification did not pass; no record was written and Wayland is served on the previous one. | Read the logged failing steps; not retried for 6 h, nor again in this boot. |
| `held_this_boot` (`x11_requalify`) | An attempt already finished in this boot; not repeated until the host reboots. | `arcen-pier repair-x11-unix-dir` re-arms it, or requalify by hand. |
| `held_recent_failure` (`x11_requalify`) | An attempt on this stack failed less than 6 h ago (or three were interrupted in a row). | Fix the failing step, requalify by hand. |
| `disabled` (`x11_requalify`) | `capture.wayland_auto_requalify` is `false`. | Requalify by hand. |
| `after_record_version` (`x11_requalify`) | This start already re-qualified the host after an update. | Nothing; the next start re-proves X11. |

## Grading and HDR refusals

Grading, HDR and colour-fidelity requests need a hardware ten-bit encoder; the
software fallback is H.264 8-bit 4:2:0 only, so a Pier that cannot use its GPU
encoder refuses these requests instead of serving eight bits under a ten-bit
name (Auto and Speed degrade visibly to Software instead). The refusal names
why the GPU encoder was not used. A Windows Pier leads the Deck's sign-in error
with the code (`gpu_sessions_in_use: Grading refused: every GPU encoder session
is in use; …`); a Linux Pier closes the connection with the same sentence
(the close reason has no room for the code). The Deck's **Colour check** shows
it under Session as "last connection ended". The Pier logs the GPU's own
`UNAVAILABLE` line with it.

| Code | Meaning | What to do |
| --- | --- | --- |
| `software_encoder_only` | The host only has the software encoder: `video.encoder` is `software-h264`, or no GPU encoder was tried for this display. | Set `video.encoder` to `auto` on a host with a supported NVIDIA GPU, or connect with Auto or Speed. |
| `gpu_display_unsupported` | The GPU path cannot capture this desktop at its size or depth: on Windows the display cannot give a ten-bit (FP16 scRGB) capture surface; on Linux the desktop is over the ten-bit capture budget or eight-bit. | Use a smaller Deck size (turn off Retina streaming for this connection) or a single display; on Windows check that the session display is the Arcen virtual display. |
| `gpu_sessions_in_use` | Every GPU encoder session is in use — a consumer GPU's concurrent-session limit, or other encoders on the same GPU. | Close other encoding sessions (other Arcen sessions, recorders, streaming software), or use a GPU without a session limit. |
| `gpu_encoder_unavailable` | No usable GPU encoder for this display: no NVIDIA adapter owns it, the driver failed to open the encoder, or a vGPU is unlicensed. | Check `nvidia-smi` (licence state on a vGPU, driver loaded) and that the configured adapter is the NVIDIA one. |
| `gpu_runtime_missing` | The GPU encoder's runtime library (`nvEncodeAPI64.dll`, `libnvidia-encode`) is not installed. | Install or repair the NVIDIA driver. |
| `gpu_format_unsupported` | The GPU encoder cannot encode the ten-bit format the request needs (HEVC Main 4:4:4 10 for Grading, Main10 for HDR). | Use a GPU generation that encodes HEVC 4:4:4 10-bit (NVIDIA Turing or later), or connect with Auto or Speed. |
| `gpu_encoder_not_built` | This Pier build has no GPU encoder. | Install the release build, which includes NVENC. |

## Windows HDR session refusals

A Windows Pier sets up an HDR session's display before it answers the Deck,
inside the broker's 30-second readiness wait. When HDR cannot be shown it
restores the display and its HDR state and refuses with one of these codes,
instead of being stopped by the broker (which the Deck saw as
"connection timed out" and the Pier logged as `WATCHDOG_RESTORE`, 1204).

| Code | Meaning | What to do |
| --- | --- | --- |
| `hdr_not_activated` | Windows did not activate HDR on the session display within the setup budget (25 s from the agent's start, 5 s short of the broker's wait). The display and its HDR state were restored. | Connect with Grading, or try HDR again. If it persists, check that Windows HDR can be switched on for the virtual display (Settings › Display) and read the agent log at `logging.level` 3 for `advanced colour state verification`. |
| `hdr_link_exceeded` | The requested display size cannot carry ten-bit HDR10 at or above the HDR stream rate over the virtual display's link (550 MHz). A single display is instead created at a lower refresh; this refusal is for sizes no such refresh fits — at sign-in or on a live resize, which keeps the current size — and for a multi-monitor monitor whose exact refresh does not fit. | Use a smaller Deck size (turn off Retina streaming for this connection), a single display, or Grading. |

## Linux installer refusals

The Linux installer chooses Xorg or Wayland from the host's facts and refuses
when it cannot serve either.

| Code | Meaning | What to do |
| --- | --- | --- |
| `xorg_override_without_xorg` | `capture.provider` is `xorg` and no X server is installed. | Install Xorg, or remove the override. |
| `wayland_not_built` | Wayland is needed and the embedded Pier was built without it. | Use the Wayland-enabled installer. |
| `no_display_stack` | No X server and no usable DRM render node. | Install the NVIDIA driver and either Xorg or GNOME. |
| `render_node_restricted` | Render nodes exist but only a group can open them. | Make `/dev/dri/renderD*` world-accessible (the distribution default) or install Xorg. |
| `wayland_incomplete` | Wayland is needed and a prerequisite is missing. | Install what the installer lists. |

## Transport lane refusals

A Pier and Deck carry one session over several QUIC streams ("lanes"). These
refusals mean one side opened a lane the other would not accept; the session
usually carries on. Persistent refusals mean mismatched builds.

| Code | Meaning | What to do |
| --- | --- | --- |
| `attachment_ended` | The session attachment had already ended. | Nothing; a late lane after a disconnect. |
| `control_not_first` | A lane arrived before the control lane. | Update the Deck and Pier to the same release. |
| `pre_authorization` | A lane arrived before sign-in authorised it. | As above; repeated from one address, treat as hostile. |
| `wrong_shape` | The stream shape does not match its role. | Update both sides to the same release. |
| `wrong_direction` | The lane was opened by the side that may not open it. | Update both sides to the same release. |
| `role_not_granted` | The attachment did not grant this role. | Update both sides; check that the feature (for example audio) is enabled on the Pier. |
| `stale_attachment` | The lane names an older attachment. | Nothing; a reconnect raced a late lane. |
| `attachment_mismatch` | The lane names another attachment. | Nothing if rare; otherwise update both sides. |
| `duplicate_lane` | The role already had an open lane. | Update both sides to the same release. |
| `stale_lane_generation` | The lane generation is not newer than the last. | Nothing if rare. |
| `lane_disabled` | The role was disabled for this attachment. | Check the Pier's policy for that feature. |

## Log events that need attention

Warning and error events from the JSON Lines log. Alerting should key on
`event_id`; see [Monitoring](monitoring.md) for the schema, profile levels and
recipes. Informational events are listed there.

| Event | Meaning | What to do |
| --- | --- | --- |
| 1002 `SERVICE_FAILED` | **Error.** The Pier service failed to start or stopped on an error (`stage`, `reason_class`). | Run `arcen-pier validate-config --config <path>`; read the start-up log. |
| 1101 `SESSION_AUTH_FAIL` | **Warning.** A sign-in was refused, or a client left before signing in (`stage` `await_auth_response`, `reason_class` `transport_error`, `client_disconnected` or `timeout` — a Deck's connection test looks like this). | See [Linux sign-in refusals](#linux-sign-in-refusals); on Windows, the credential-provider or `LogonUser` stage. |
| 1104 `SESSION_INTERRUPTED` | **Warning.** A session dropped without an orderly close. | Check the network path; the Deck reconnects within `auth.reconnect_window_secs`. |
| 1202 `DISPLAY_RESTORE_DEGRADED` | **Warning.** The host's displays were restored, but not exactly. | Check the host's display layout. |
| 1203 `DISPLAY_RESTORE_FAILED` | **Error.** The host's displays could not be restored after a session. | Restore the layout by hand; read the display-restore log; report it. |
| 1204 `WATCHDOG_RESTORE` | **Warning.** The watchdog restored displays after a crashed session. | Read the session log for the crash. |
| 1301 `CP_LOGON_FAIL` | **Warning.** The Windows credential provider could not sign the user in. | Check the account; see "Reading the log when a sign-in is refused" in the administration guide. |
| 1401 `TLS_CERTIFICATE_EXPIRING` | **Warning.** The Pier's certificate expires within `tls.expiry_warning_days`. | Renew it; keep the key to keep the SPKI fingerprint Decks remember. |
| 1403 `TLS_CERTIFICATE_RELOAD_FAILED` | **Error.** A reloaded certificate was refused; the old one stays active. | Fix the new PEM (key match, SANs, key type) and reload again. |
| 1404 `TLS_CERTIFICATE_EXPIRED` | **Error.** The Pier's certificate has expired; Decks will refuse it. | Renew it now (rerun the installer, or `arcen-pier new-host-cert` on Linux and macOS). |
| 1504 `CLIENT_CONNECT_FAIL` | **Error** (Deck log). A connection failed; `reason_class` says how. | Run **Test connection** for the same address. |
| 1506 `CLIENT_RECONNECT` | **Warning** (Deck log). The Deck is reconnecting. | Nothing if it recovers; otherwise check the path. |
| 1604 `HID_PASSTHROUGH_ERROR` | **Error.** USB/HID passthrough failed for a device. | See the USB section of [Troubleshooting](troubleshooting.md#usb-devices). |
| 1702 `NETWORK_PATH_LOST` | **Warning.** The network path to the peer was lost. | Check the network; the Deck reconnects. |
| 1801 `HEALTH_DEGRADED` | **Warning.** Frame rate, round trip, drops or input latency passed the degraded threshold (`logging.qos_targets`). | See "Laggy" in [Troubleshooting](troubleshooting.md#laggy-or-stuttering). |
| 1802 `HEALTH_CRITICAL` | **Error.** A QoS measure passed its critical threshold. | As for 1801; check host GPU load and the link. |
| 1803 `HEARTBEAT_LOST` | **Error.** The peer stopped answering heartbeats. | Check the network; the session ends or resumes. |
| 1804 `TELEMETRY_DROPPED` | **Warning.** Log records were dropped under load. | Lower `logging.level` or give the log volume more space. |
| 1901 `PERMISSION_DENIED` | **Warning.** An operating-system permission is missing (macOS: Screen Recording, Accessibility, Local Network, microphone). | Grant it in System Settings › Privacy & Security and restart the app. |
| 1902 `PERMISSION_REVOKED` | **Warning.** A permission was withdrawn while running. | Grant it again. |
| 2000 `LANE_REFUSED` | **Warning.** A lane was refused (`reason_class`). | See [Transport lane refusals](#transport-lane-refusals). |
| 2001 `LANE_FAILED` | **Warning.** A lane failed mid-session. | Check the network; the session usually carries on. |
| 2002 `LANE_STARVED` | **Warning.** A lane could not keep up. | Check bandwidth and host load; choose Speed. |
| 2005 `DESKTOP_PROVIDER_UNAVAILABLE` | **Error.** The Linux Pier has no desktop provider it can serve. | See [Wayland provider blockers](#wayland-provider-blockers) and the start-up log. |
