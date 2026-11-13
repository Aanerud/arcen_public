# Troubleshooting

Organised by what you see. Each entry names the likely causes in order, how to
tell them apart, and the fix. Codes in `code` style are explained in
[Error codes](error-codes.md); configuration keys in the
[configuration reference](config-reference.md). The
[administration guide](pier-administration.md) explains installation and the
policies behind the keys.

**Start with the connection test.** It answers most "it does not work"
questions in five seconds and needs no password: in the Deck, open a
connection and choose **Test connection** (also in the connection editor), or
from a terminal:

```sh
"/Applications/Arcen Deck.app/Contents/MacOS/arcen-deck" diagnose <host[:port]>        # Apple silicon
"/Applications/Arcen Deck Intel.app/Contents/MacOS/arcen-deck" diagnose <host[:port]>  # Intel
```

It checks name resolution, UDP reachability with a real QUIC handshake, the
certificate against the identity this Deck remembers, the Pier's greeting
before sign-in, the path MTU and the round trip, and says what to do about
each failure. **Copy report** puts the whole result on the clipboard for a
support ticket. For the Pier side, a [support bundle](support-bundles.md)
collects the logs.

- [Cannot connect](#cannot-connect)
- [Sign-in refused](#sign-in-refused)
- [Black or frozen screen](#black-or-frozen-screen)
- [Laggy or stuttering](#laggy-or-stuttering)
- [Wrong or washed-out colours](#wrong-or-washed-out-colours)
- [The pen acts as a mouse](#the-pen-acts-as-a-mouse)
- [Keyboard types the wrong characters](#keyboard-types-the-wrong-characters)
- [Cannot paste into the sign-in screen](#cannot-paste-into-the-sign-in-screen)
- [No sound, or no microphone](#no-sound-or-no-microphone)
- [Clipboard does not work](#clipboard-does-not-work)
- [USB devices](#usb-devices)
- [More than one display](#more-than-one-display)
- [The Pier will not start](#the-pier-will-not-start)

## Cannot connect

Run **Test connection**; the first failing step decides.

| The test says | Cause | Fix |
| --- | --- | --- |
| `diag.dns.failed`, `diag.dns.no_addresses` | The name does not resolve from this Mac. | Use the IP address, or join the network or VPN where the name exists. |
| `diag.udp.timeout` | Nothing answers on UDP 18444: the Pier is down, the port is different, or a firewall drops UDP. Arcen uses UDP only; a TCP port check proves nothing. | On the Pier: Linux `systemctl status arcen-pier`, Windows `Get-Service ArcenPier`. Compare `listen.port`. Allow inbound UDP 18444 on the host firewall (the Linux and Windows installers add it on recognised firewalls) and on every network firewall and VPN in between. |
| `diag.udp.refused` | The machine is up but nothing listens on that port. | Start the Pier service, or use the port in `listen.port`. |
| `diag.udp.local_network_unreachable` | macOS blocks Arcen Deck from the local network, or the Pier is off. | System Settings › Privacy & Security › Local Network: enable Arcen Deck. |
| `diag.udp.transport_mismatch`, `diag.greeting.unexpected` | The Deck and Pier are from releases that do not speak the same transport. | Update both to the same release. |
| `diag.cert.mismatch` | The Pier presents a different identity from the one this Deck remembers: a reinstalled Pier, a replaced certificate, or an impostor. | **Do not sign in.** Compare the SPKI fingerprint in the report with `host.spki-sha256` in the Pier's configuration directory. Only if they match, choose **Forget and Verify** when the Deck offers it. |
| `diag.cert.expired`, `diag.cert.not_yet_valid` | The Pier's certificate is out of date, or a clock is wrong. | Renew it (rerun the installer; `arcen-pier new-host-cert` on Linux and macOS) and check both clocks. |
| `diag.cert.name_mismatch` | The certificate does not list the name or address you typed. | Connect with a name it lists, or reissue it with that name. |
| `diag.mtu.too_small` | A VPN or tunnel interface below 1228 bytes cannot carry QUIC's 1200-byte packets. | Raise the tunnel MTU to at least 1280. |
| Everything passes, connecting still fails | The failure is after sign-in. | See [Sign-in refused](#sign-in-refused) and the Deck's error text. |

A Deck that keeps saying "Reconnecting…" after a network change is resuming
the same session with its grant; if the grant is refused it asks for the
password again (see the resume codes in [Error codes](error-codes.md#session-resume-refusals)).

## Sign-in refused

The Deck shows the Pier's reason; the Pier logs `SESSION_AUTH_FAIL` (1101) with
a `reason_class` — look it up in [Error codes](error-codes.md). Common cases:

| You see | Cause | Fix |
| --- | --- | --- |
| Authentication failed | Wrong account or password, or the operating system's lockout policy (`pam_faillock`, Windows account lockout) is holding the account. | Sign in at the console to confirm the account; wait out the lockout. Arcen never stores or checks passwords itself. |
| "The host signed you in but could not start the session" | The password was right; the desktop could not start. | The Pier log names the stage: `xorg_start_failed`, `wayland_start_failed`, `pam_session_failed`, `selinux_context_refused`, `local_playback_refused`, … |
| Every Linux session fails with `dedicated Xorg failed to start` | The Xorg template's `BusID` names no NVIDIA GPU on this host, or the `nvidia` driver is not loaded (`(EE) No devices detected` in `/run/arcen/sessions/<sid>/Xorg.log`). | Rerun the installer to set `BusID`, or edit `/etc/arcen/xorg.conf` (decimal: `0000:00:10.0` is `PCI:0:16:0`). See "The NVIDIA GPU in `/etc/arcen/xorg.conf`" in the [administration guide](pier-administration.md). |
| `selinux_context_refused` | The `arcen` SELinux module is not loaded, or the user maps to a confined SELinux user. | `semodule -i /usr/share/arcen/selinux/arcen.cil` or rerun the installer. Never `setenforce 0`. See [Linux Pier under SELinux](../security/linux-selinux.md). |
| "Another remote desktop is active on this host (one X11 desktop per host)" (`seat_occupied`) | Another X11 desktop holds `seat0` (another user, or a desktop still closing). `loginctl list-sessions` shows it. | Sign out of it or let it reach its idle timeout. Earlier Piers said `session launcher protocol failed`. |
| "The host refused this desktop: it could not keep it from local administrator rights (polkit)" | `polkit_rule_missing`, `polkit_grants_remote_session` or `polkit_unverified`. | `arcen-pier polkit-rule status`; `arcen-pier polkit-rule install` and restart the Pier. See "polkit" in the [administration guide](pier-administration.md). |
| "The host could not mute its own speakers" | `audio.local_playback` is `muted` and the host cannot prove its speakers silent (macOS: System Audio Recording not granted). | Grant the permission (macOS) or fix the user's audio server (Linux); or set `audio.local_playback` to `audible`. |
| The waiting desktop belongs to another user | `another_users_desktop`. | Wait for `auth.reconnect_window_secs` to pass or for that user to sign out. |
| A notice must be accepted first | `auth.disclaimer.enabled` is on. | Read and accept it in the Deck; the command-line smokes stop here by design. |
| Windows: sign-in works but lands on the wrong session or a lock screen | Session topology (RDP, a console user, account switching). | See "Sessions, RDP and account switching (Windows)" in the [administration guide](pier-administration.md). |

## Black or frozen screen

| Cause | How to tell | Fix |
| --- | --- | --- |
| Windows first sign-in is still settling | About 25 seconds after sign-in; the log shows the post-login stability wait. | Expected: Windows first login waits 15 s for Explorer and DWM before capture so it does not stream black frames. |
| A Windows host with two GPUs streams from the wrong one | `no attached capture-capable outputs matched adapter filter` in the agent log, or a black desktop after a fresh configuration. | List the GPU kept for other work in `platform.multi_monitor.excluded_adapters`, and restart `ArcenPier`. See "Choosing which GPU encodes (Windows)". |
| A lock screen or sleeping display on the host | The Deck shows the host's lock screen or a blank output; after a move between sessions Windows locks. | Unlock it; see "After a move, expect a lock screen" in the [administration guide](pier-administration.md). |
| macOS Pier without Screen Recording | `PERMISSION_DENIED` (1901) in the Pier log. | Grant Screen Recording to the Pier in System Settings › Privacy & Security and restart it. |
| Linux Wayland qualification is stale after an update | The Pier refuses Wayland desktops and names a stale reason (`compositor_version`, `gpu_driver`, …). | `arcen-pier wayland qualify --write`, or leave `capture.wayland_auto_requalify` on. |
| X11 applications missing on a Linux Wayland desktop, or every GNOME desktop aborting at start (gnome-shell SIGTRAP) | `/tmp/.X11-unix` is owned by a user, not root (Mutter recreated it after systemd's daily clean had removed it on an idle host). The Pier logs the fault as an error at start (`x11_unix_dir_foreign_owner`, or `_not_shared`, `_not_a_directory`) and `this desktop starts without X11 applications` at each sign-in; a qualification skips X11 and records the fault as a host precondition. | As root, `arcen-pier repair-x11-unix-dir` (`--check` only reports); it fixes owner, mode and label of `/tmp/.X11-unix` and `/tmp/.ICE-unix` with the system's own tmpfiles entry. Remove that user's stale `X<n>` sockets and `/tmp/.X<n>-lock` files once none of their X servers runs. When the record skipped X11 (`x11_precondition_cleared`), the Pier re-proves X11 applications itself at its next start, once per boot (`x11_requalify_outcome` in the log); `repair-x11-unix-dir --requalify` restarts it now, ending running desktops. A missing directory the Pier recreates itself, and the installer's `/etc/tmpfiles.d/arcen-pier.conf` stops the daily clean removing it. See [error codes](error-codes.md#x11-applications-in-wayland-sessions). |
| The ten-bit presentation path failed on the Deck | A persistent warning on the session window: the Deck fell back to eight-bit presentation. | Reconnect; if it persists, choose Auto and report the Deck log. |
| Black bars after a resize, and the host does not follow the window | The hello says `supports_display_update:false`; display logs say `retarget custom timing enumeration unavailable`. | A GPU limit on some GRID vGPU hosts (no custom display timings), not a configuration problem. |
| A desktop that does not change | Nothing is wrong: Arcen sends only what changed, so a still desktop sends no new frames and the Deck keeps the last one. | Move the pointer; if the picture never updates, run the connection test and read `SESSION_STREAM_START` (1102) in the Pier log. |

## Laggy or stuttering

Measure before changing anything: **Test connection** reports the round trip
against the Pier's own thresholds (`logging.qos_targets`: degraded above
60 ms, critical above 150 ms), and the Pier logs `HEALTH_DEGRADED` (1801) and
`HEALTH_CRITICAL` (1802) with the measure that crossed.

| Cause | How to tell | Fix |
| --- | --- | --- |
| A long or hairpinned network path | `diag.rtt.degraded` or `diag.rtt.high`; a VPN interface in the path step. | Use a closer or wired path; a VPN that routes through a distant site adds every millisecond. |
| Wi-Fi contention | Round trip varies between runs; drops in 1801/1802. | Use Ethernet, or a 5/6 GHz band close to the access point. |
| The preset asks for 30 fps | Auto runs at 30 fps. | Choose **Speed** (60 fps); on a macOS Pier the difference is large (see "If it feels slow, ask for sixty" in the [README](../../README.md)). |
| The host GPU is busy, or its encoder sessions are full | `ENCODER_ADMISSION_FPS_REDUCED` (1904) or `PRESET_ADMISSION_DECIDED` (2003): the session was admitted at a lower rate. Consumer GeForce cards limit simultaneous encode sessions; each monitor is one. | Fewer monitors, a professional GPU, or `allow_software_fallback` for overflow monitors. |
| Host applications render in software on a depth-30 Xorg desktop | Firefox shows *WebRender (Software)*; Chrome and Electron composite in software. | Leave `video.x11_desktop_depth` on `auto`, or use the per-application settings in "Linux desktop depth" in the [administration guide](pier-administration.md). |
| A lane cannot keep up | `LANE_STARVED` (2002). | Check bandwidth; choose Speed. |

## Wrong or washed-out colours

Start with **Session › Colour Check…** in the Deck: it compares what the host
serves with what was asked for, checks this Mac's display, presentation layer,
EDR and scaling, measures a test pattern code for code, and offers the fix —
see the [colour check guide](colour-check.md). Copy its report into any
support request.

| Cause | How to tell | Fix |
| --- | --- | --- |
| Auto and Speed are eight-bit 4:2:0 | Coloured fringes on fine text and thin lines. | Choose **Grading** (HEVC 4:4:4 ten-bit, BT.709) for colour work. On Windows, `platform.font_smoothing: grayscale` removes ClearType's colour fringes. |
| A Linux desktop created at depth 24 cannot serve Grading | The Deck says the session was "served visibly as Auto" (`eight_bit_source`) and tells you to sign out. | Sign out and reconnect with Grading, or set `video.x11_desktop_depth` to `30` on grading hosts. |
| HDR on an Xorg host | Xorg cannot report HDR, so HDR resolves to Grading (SDR, BT.709) and the Deck says so. | Expected. If a colour-managed application really writes Rec.2100 PQ, declare it with `video.desktop_encoding: rec2100-pq`. |
| The Pier's colour policy forces the baseline | `video.color_policy: always-off` serves every session eight-bit, limited range, BT.709. | Use `default-off` (the default) or `default-on`. |
| HDR is bright or flat on the Mac | EDR is used only when the Pier returns PQ; Grading is SDR by design. | Check the session details: transfer `pq` means HDR; `bt709` is SDR. |
| The picture is scaled on the Mac | Colour check › Scale is not 1:1 (`colour.scale.resampled`). Resampling blends neighbouring pixels. | Enter fullscreen, or turn on Retina-resolution streaming (both one click in the colour check). |
| The Mac display adapts its brightness or white point | Colour check › Display reports no reference mode (`colour.display.not_reference`). | Use a reference preset (System Settings › Displays › Preset) or a calibrated display; turn off True Tone and automatic brightness. |

Each preset is a separate capture pipeline; see
[Streaming presets and independent pipelines](../features.md#streaming-presets-and-independent-pipelines).

## The pen acts as a mouse

| Cause | How to tell | Fix |
| --- | --- | --- |
| The connection's tablet mode is **Mouse compatibility only** | **Settings › Input › Tablet** (saved with the connection selected in the Connections list). | Choose **Tablet support** and connect again; a mode change takes effect on the next connection. |
| A headless Linux Wayland desktop | GNOME's input emulation has no stylus yet, so pen events arrive as a mouse. | Let the desktop run natively on its own seat (`wayland.session_mode: auto` with a qualified native seat; one per host). See [Native Wayland seat](error-codes.md#native-wayland-seat) for why a seat was not available. |
| Native tablet (USB bridged) on a host that cannot take it | Windows, headless Wayland desktops and Linux hosts that have not enabled it refuse it, and the Deck says why and keeps mouse control. | Use **Tablet support**, or on a Linux host enable it (`install-arcen-pier --native-tablet`; [Native tablet](pier-administration.md#native-tablet-usbnative_tablet)). Windows hosts never offer it ([ADR 0017](../adr/0017-forward-only-pen-input.md)). |
| An application that reads only mouse events, or a Windows application set to Wintab | Pressure works in other applications. | Enable the application's tablet input. On Windows choose Windows Ink: per-application settings are in [Windows applications: choose Windows Ink](pen-tablet-input.md#4a-windows-applications-choose-windows-ink). Arcen does not support Wintab, so an application with no Windows Ink mode (Mari 4.7 and earlier) gets no pressure ([ADR 0017](../adr/0017-forward-only-pen-input.md)). |

Validation steps per host are in [Pen and tablet input](pen-tablet-input.md).

## Keyboard types the wrong characters

| Cause | How to tell | Fix |
| --- | --- | --- |
| The host session's keyboard layout differs from the Mac's | Letters land in the wrong places (`z`/`y`, `æøå` as punctuation). Keys are sent by position and the host's layout decides the character. | Switch the session to **Connection › Keyboard: Characters** (⌃⌘U; ⌃⌘K goes back), or save the connection with Keyboard mode **Characters**: every typed character then arrives as on the Mac. Or set the host session's layout to match the Mac. Needs a Windows, Linux Xorg or Linux Wayland Pier from 0.18.0. |
| Characters mode is on where keys are needed | A game, a VM console or an application that reads keys misbehaves; the video shows **Keyboard: Characters**. | ⌃⌘K (Physical Keys). |
| Option is Alt, or Option types characters, the wrong way round | `Option` shortcuts or special characters misbehave. | Settings › Input: choose how the left and right Option keys behave (**Automatic**, the default, keeps both as Option on a Mac host); on a Windows host right Option is AltGr (Edit connection › **Left Option is also AltGr** for the left one). |
| `⌘` shortcuts do not do what you expect | Each host gets its own mapping: on Windows ⌘ is the Windows key except ⌘C/X/V/A/Z/F/W, on Linux ⌘ is Ctrl. | Edit connection › **Keyboard shortcuts**; see the README, "Your Mac keyboard on the host". |
| Characters outside the session layout on GNOME 49 | The Deck reports characters the host could not type. | The compositor lacks libei text input; type them on the host's layout. |

## Cannot paste into the sign-in screen

Clipboard sharing reaches the signed-in desktop, not a sign-in, lock or
elevation screen, a virtual-machine console or a firmware-style application.
Use **Connection › Paste as Keystrokes** (⌃⌥⌘V): it types the Mac clipboard's
plain text there.

| Cause | How to tell | Fix |
| --- | --- | --- |
| The host cannot type text | "This host cannot type text sent from the Deck…" | Update the Pier to 0.18.0 or later (Windows, Linux Xorg and Wayland). The macOS Pier does not type text yet. The Deck never falls back to key positions, which a different layout would mistype. |
| The host's clipboard policy keeps the Mac's clipboard out | "This host's administrator does not allow text from this Mac's clipboard…" | `clipboard.direction` excludes Mac-to-host text; Paste as Keystrokes honours it by design. |
| Nothing happens until you let go | **Release the modifier keys to start typing**. | Typing waits while any modifier is held, so no character becomes a shortcut. |
| A password did not sign in | The field shows the right number of dots but sign-in fails, or the screen's curtain was still down. | Click the password field (or press a key to lift the lock-screen curtain) first, then paste. The line break at the end of copied text is never typed — press Return yourself. |
| Characters are missing in a slow console | Some characters did not arrive. | Settings › Input › Paste as Keystrokes › **Typing speed**: 10 or 25 characters a second. |
| Characters were refused | "N characters were not typed: …" | On GNOME 49 Wayland, characters outside the session layout (on a UK layout: `å`, `é`, `ü`, emoji, Cyrillic; dead-key accents on any layout) need libei text input, which GNOME 49 does not offer; add the layout that has them as the session's input source, or type them on the host. When the session's keymap holds several different layouts — several input sources, or GNOME's extra layout for the locale (a Norwegian source on an English-locale host is `no,gb`) — GNOME does not say reliably which one is active, so only characters every layout types on the same keys are typed by key; the Pier log line "session keyboard layout read" shows the groups. The Pier types only what the session layout proves, so a character is refused rather than typed wrong. The characters are counted, never shown. |
| It types into the wrong place | Typing goes wherever the host's keyboard focus is. | Esc stops it at once; click the right field and paste again. Keys you press while it types are not sent. |

## No sound, or no microphone

| Cause | How to tell | Fix |
| --- | --- | --- |
| Audio is off on the Pier | `audio.enabled` is `false`. | Set it to `true` and restart. |
| Windows has no default playback device | Video works, the agent log reports no render endpoint for loopback. | Give the host a default audio output. |
| Linux audio capture is waiting | The audio helper reports a capture gap while PulseAudio's monitor is suspended. | Play something on the host; the gap closes. |
| macOS Pier without System Audio Recording | The session is refused with "could not mute its own speakers", or no audio. | Grant System Audio Recording to the Pier's helper. |
| The Mac's sound is muted, turned down, or going to another output | The Deck has no volume or mute control of its own: it plays through the Mac's default output device, chosen when the session's sound starts. Session › Connection Health shows the audio line, so sound arriving there but not heard is on the Mac side. | Unmute or raise the Mac's volume (menu bar or Control Centre › Sound) and check the output in System Settings › Sound › Output. After changing the output device, disconnect and connect again. |
| Microphone: not enabled this launch | The microphone is opt-in on the Deck every launch, by design. | Settings › Microphone › **Allow microphone forwarding for this launch**, then connect again. |
| Microphone: the Pier refuses it | `microphone_input.enabled` is `false`, or Windows lacks the optional driver Arcen does not ship. | Enable it on Linux or macOS; see [Audio input](audio-input.md). |

## Clipboard does not work

| Cause | How to tell | Fix |
| --- | --- | --- |
| Pier policy | `clipboard.direction` is `disabled` or one-way; `clipboard.content` excludes images or text. | Change the policy and restart. The host decides; the Deck cannot override it. |
| The copy is too large | A host copy above `clipboard.max_bytes` (8 MiB by default, at most 20 MiB) is refused and the session goes on. | Raise `clipboard.max_bytes` or copy less. |
| A Wayland desktop whose clipboard could not be proved | The hello advertises clipboard disabled; the session log has `clipboard_bridge_unavailable` or `clipboard_bridge_lost`. | Reconnect; a lost bridge turns the clipboard off for that attachment only. |
| Not negotiated | Clipboard starts only after clipboard v1 is negotiated for an authenticated session. | Update the Deck and Pier to the same release. |

See [Clipboard redirection](../security/clipboard-redirection.md) for the policy.

## USB devices

| Cause | How to tell | Fix |
| --- | --- | --- |
| The Deck's USB helper is not installed or approved | `arcen-deck usb-helper-status` (USB-enabled builds) says `requires_approval` or `not_registered`. | Approve it in System Settings › General › Login Items, or install it. |
| The host cannot take a bridged device | Native tablet (USB bridged) runs on Linux Piers whose administrator enabled it (`usb.native_tablet`), and on macOS Piers (pressure for the models the Pier reads itself, the Intuos5 touch L today), on a LAN. Windows hosts do not offer it: it needs a signed kernel driver the project does not fund. | Use Tablet support for pens; see [ADR 0017](../adr/0017-forward-only-pen-input.md). |
| A passthrough failure | `HID_PASSTHROUGH_ERROR` (1604; logged only at `logging.level` 1 or higher). | Raise the level, reproduce, and read the event's fields. |
| Webcams | Webcam redirection is not supported. | — |

## More than one display

| Cause | How to tell | Fix |
| --- | --- | --- |
| "Match My Layout could not be negotiated with this host" | The Pier's greeting has no Match My Layout offer — **Test connection** prints "Match My Layout not offered". | `platform.multi_monitor.advertise_enabled` is `false`; Linux found no usable NVIDIA heads; Windows found no eligible NVIDIA/NVENC adapter after `allowed_adapters`/`excluded_adapters`, or that adapter has no attached or provisionable outputs. Run `arcen-pier diagnose-host` in an interactive Windows session. Set `max_monitors` to a truthful ceiling on hosts that cannot serve the Deck's full layout. |
| Fewer monitors than asked | Encoder admission reduced the set. | See [Laggy](#laggy-or-stuttering) for encoder-session limits. |

## The Pier will not start

| Check | Cause | Fix |
| --- | --- | --- |
| `arcen-pier validate-config --config <path>` | An unknown key, a missing `audio` or `microphone_input` section, or a value out of range. | Fix the key it names; see the [configuration reference](config-reference.md) (a key newer than the installed Pier is refused too). |
| TLS errors in the service log (Linux `journalctl -u arcen-pier`, Windows service log) | Missing PEM, key and certificate that do not match, a certificate without SANs, an unsupported key, an expired certificate, or a Windows key readable by too many accounts. | Fix the material, then reload or restart; `TLS_CERTIFICATE_RELOAD_FAILED` (1403) keeps the old certificate. |
| A QUIC bind failure | Another process owns the UDP port, or the listen address does not resolve. | Free UDP 18444 or change `listen.port`/`listen.host`. |
| Linux: `DESKTOP_PROVIDER_UNAVAILABLE` (2005) | No desktop provider it can serve (no Xorg, Wayland not qualified). | See [Wayland provider blockers](error-codes.md#wayland-provider-blockers). |
| Capture or encode fails at the first session | The capenc READY line and `SESSION_STREAM_START` (1102) fields. `not_built` means the backend was not compiled in; `capenc READY protocol error: missing cursor` means an old Pier binary. | Reinstall the matching Pier build. Exact codec or `video.variant` pins refuse substitution by design. |
| The health snapshot says `overall_state: unavailable` during a working session | `HEALTH_SNAPSHOT` (1806). | Known: do not alert on `overall_state` yet; use `SESSION_STREAM_START` and the QoS fields. |
