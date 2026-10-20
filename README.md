# Arcen

**The span between you and your machine.** Arcen streams a real desktop from a
powerful workstation to a thin client, over an encrypted direct connection.

Native Rust throughout. Free software under the **AGPL-3.0**.

> **There is no support.** Arcen is published because publishing it is more
> useful than letting it rot in a private repository. No company stands behind
> it and nobody is on call. See [SUPPORT.md](SUPPORT.md).

---

## What works today

Read this before you invest time.

| | State |
|---|---|
| **Linux Pier** (host) | Works, on **X11** and — new in 0.17.0 — **Wayland** (GNOME Shell/Mutter). The Pier chooses from what the host has, never from the distribution's name: an installed Xorg server keeps the dedicated X11 desktop, and an existing installation never moves; without Xorg, each sign-in gets its own GNOME desktop, which the installer qualifies on the host before the service starts and the service re-qualifies after updates. **X11:** Auto and Speed on the NvFBC GPU path (Speed at 60 fps on a head paced at 120 Hz), Grading on a separate depth-30 XShm path for genuine 10-bit SDR, HDR visibly served as Grading unless the operator declares a Rec.2100 PQ desktop; one X11 desktop per host. **Wayland:** verified end to end on Rocky Linux 10.2 / GNOME 49 with an NVIDIA GRID vGPU (GNOME 40 on Rocky Linux 9 is a tested baseline); other GNOME hosts are admitted only by the same qualification. Auto everywhere, Speed where qualification proves it, Grading and HDR on GNOME/Mutter 50 or newer, and a real pen with pressure and tilt on stock GNOME when the desktop runs natively on its own seat (one per host). Everything Wayland cannot do yet is served visibly lower, with the reason — see [X11 and Wayland compared](#linux-pier-x11-and-wayland-compared) and [How Wayland support works](#how-wayland-support-works). On both, the Pier keeps the host from sleeping while it serves a desktop, and a remote desktop gets polkit's remote answers, never the console's, so its user cannot power off, reboot or suspend the host, change its networking or mount disks without an administrator (fixed in 0.17.0; polkit older than 0.106 cannot be given Arcen's rule, and the Pier logs a `SECURITY:` error there — see [CHANGELOG](CHANGELOG.md)). |
| **Windows Pier** (host) | Works. Multi-display is automatic: the service picks an eligible NVIDIA adapter at start, and an administrator can reserve a GPU for other work. Auto/Speed use the 8-bit capture path; Grading uses WGC FP16 scRGB converted to 10-bit BT.709; HDR provisions and verifies an HDR head before a separate FP16-to-PQ/BT.2020 encode path. |
| **macOS Deck** (client) | Works. Signed and notarised. On Apple silicon VideoToolbox hardware-decodes H.264 and HEVC, including 4:4:4 10-bit. A dedicated 10-bit Metal layer presents Grading in SDR and enables PQ/EDR only for a host-confirmed HDR stream. Two apps from one source, both for macOS 12 or later: **Arcen Deck** for Apple silicon and **Arcen Deck Intel** for Intel Macs. AV1 decode needs Apple silicon M3 or later (macOS 14); the Intel app never offers AV1, and on Intel VideoToolbox may decode in software. Native tablet needs macOS 13. |
| **Linux Deck, Windows Deck** | Do not exist. [Help wanted.](#where-help-is-wanted) |
| **macOS Pier** | Preview. A signed, notarised package ships with each release, and a logged-in user can serve a real Deck: PAM sign-in, ScreenCaptureKit → VideoToolbox video in Auto, Speed and Grading, HDR when the display being captured has HDR headroom, one virtual display per Deck display, pointer/keyboard/pen input and trackpad gestures, clipboard both ways, host audio, and session-scoped time-zone redirection for newly launched apps. It installs as a network service under a hidden `_arcen` account plus a helper in each signed-in session ([why two parts](docs/security/macos-pier-process-model.md)). A virtual microphone (an AudioServerPlugIn) ships in the package and negotiates with the Deck, but end-to-end microphone capture has not been qualified. Not yet: deskside privacy, native tablet, and qualification of cold-boot login-window operation and of multi-display on physical hardware. FileVault's pre-boot unlock screen cannot be served. |
| **Gateway (internet traversal)** | Not shipped. It was never finished, so it is not published as dead code. It could return — see [below](#where-help-is-wanted). |

Every claim above was tested on real hardware, running real work, not merely
compiled:

- **Linux Pier**, Autodesk Flame workstation, GRID V100D-16Q → macOS Deck:
  Auto and Speed stayed on NvFBC; Grading captured real depth-30 pixels through
  XShm; HDR resolved truthfully to 10-bit BT.709 SDR.
- **Linux Pier, fresh install**, Rocky Linux 9.7 with Secure Boot, GRID
  V100D-16Q, Active Directory sign-in: the stock installer found the GPU,
  set the Xorg `BusID` itself and said so, and a Deck streamed Auto, Speed,
  Grading and HDR (served as Grading) with sound, on the first connection.
- **Linux Pier, headless Wayland**, Rocky Linux 10.2 / GNOME 49 with no Xorg,
  Secure Boot and SELinux enforcing, GRID V100D-16Q (580.105.08,
  `nvidia-drm modeset=1`) → macOS Deck: qualification, Auto with sound,
  microphone and input, Grading/HDR served as Auto, resize of a Windowed
  Deck, capture restart after capenc was killed, and reattachment to the same
  desktop; Speed qualified with no operator sizes (ladder 1920×1080,
  2560×1600, 3840×2160, HEVC and H.264, 60 Hz outputs) on the GPU import
  route, and served to the Deck at its 1800×1130 display.
- **Linux Pier, headless Wayland with the optional GNOME 51.1 prefix**, same
  host and GPU (lab build): Grading and HDR qualified (a 1024-code ramp
  measured with maximum error 0) and served to the real Deck GUI at their
  30 fps contract; two outputs qualified.
- **Linux Pier, native Wayland desktop on stock GNOME**, Rocky Linux 10.2 /
  Mutter 49.4, same GPU (development build of this release): the real Deck
  GUI at Speed, driven with synthetic tablet events, reached Firefox 140 and
  Chrome 155 as a pen for every inked sample, with 32 pressure levels and
  tilt, exactly one cursor in every frame, and capenc serving 60 fps over
  DMA-BUF.
- **Linux Pier, X11 and Wayland, real screen**: the real Deck GUI driven by
  real input against Firefox in an Active Directory user's desktop showed
  Speed at 60 fps on both providers, one cursor (the Mac's own), and on X11
  the pen delivering pressure and tilt in Firefox and Chrome, taps and very
  light strokes included.
- **Windows Pier**, DaVinci Resolve grading workstation, GRID V100D-16Q →
  macOS Deck: Auto, Speed, Grading, and HDR all completed end to end. Grading
  used WGC FP16 without enabling HDR; HDR used WGC FP16 with verified
  PQ/BT.2020 output state.
- **Windows Pier**, no GPU at all → macOS Deck: `openh264-sw-h264`
  source-built software encode, still hardware-decoded on the Mac.
- **Multi-display, automatic**, with nothing configured: the Linux Pier found
  four NVIDIA heads on its own and served a two-display Deck layout; the
  Windows Pier on a host with two GPUs streamed from the V100D while
  `excluded_adapters` kept the RTX 6000 free for other work. Setting
  `advertise_enabled: false` turned it off, as it should.
- **macOS Pier**, one-display lab Mac → macOS Deck: installed from the
  notarised package with Installer.app, authenticated as the console owner,
  streamed an Aqua desktop, proved clipboard in both directions, and verified
  `adaptive-performance` as `420v`/`Yuv420` and `color-fidelity` as
  `x444`/`Yuv444`. Host-audio end-to-end tests pass on a machine with consent.
- **macOS Deck on Intel**, Mac Pro (2013) on macOS 12.7.6: the Intel build
  launches, passes the Deck test suite, and decodes a 720p H.264 stream through
  the session decoder at about 150 fps in software. Its probe reports H.264,
  HEVC, 4:4:4 and Main 10 decode sessions but no AV1. Native tablet reports
  that it needs macOS 13. `Arcen Deck Intel.app` cross-compiled on an
  Apple-silicon Mac (`build-deck-app.sh --arch x86_64`) runs there too:
  `arcen-deck platform-report` reports `macos-intel`, the same decode probe,
  and no native tablet. From the Mac Pro it streamed and decoded Auto, Speed,
  Grading (10-bit 4:4:4) and HDR (PQ/BT.2020) from the Windows Pier, and
  Auto, Speed and Grading from the Linux Pier (HDR served as Grading).
- The Linux and Windows Piers in the release matrix are Active
  Directory-joined, so the credential crossing the wire is a domain credential,
  not a local account. The macOS Pier evidence above is one lab Mac, not a
  release qualification matrix.
- The release matrix also covered nonzero audio, keyboard/pointer input, both
  cursor authorities, display restore, and credential-free reconnect.

**Windows headless HDR account requirement.** The account being signed in must
currently be a member of the host's local Administrators group when Arcen has
to provision an unused NVIDIA display ID. NVIDIA NVAPI rejects that
session-scoped EDID mutation for a standard account. This limitation is
specific to the automatic NVIDIA headless-display path; it does not imply that
an attached display or the ordinary eight-bit pipeline needs an administrator
account.

### Linux Pier: X11 and Wayland compared

The same Pier binary serves either desktop; which one a host gets is decided
at start (`capture.provider`, see [How Wayland support works](#how-wayland-support-works)).

| | X11 (an Xorg server is installed) | Wayland (GNOME Shell/Mutter, no Xorg) |
|---|---|---|
| **Desktop** | A dedicated Xorg server on the console seat; one per host | Headless: a seatless GNOME session per sign-in. Native: the same stock compositor on its own logind seat, one per host |
| **Auto / Speed** | NvFBC → CUDA → NVENC, frame stays on the GPU; Speed at 60 fps on a head paced at 120 Hz | Mutter ScreenCast → PipeWire DMA-BUF → EGL → CUDA → NVENC with explicit sync, where qualification verified the import (otherwise a host copy). Speed only where qualification measured the rate at that display size, on one output; otherwise visibly Auto |
| **Grading** | Depth-30 Xorg → XShm RGB10 → shared conversion → NVENC 4:4:4 10-bit, BT.709. The desktop must be created at depth 30 (`video.x11_desktop_depth`) | GNOME/Mutter 50 or newer after a per-host precision proof, one output, HEVC 4:4:4 10-bit BT.709. GNOME 49 (Rocky Linux 10) only through the optional [side-by-side GNOME prefix](docs/operations/pier-administration.md#upgrading-the-gnome-session-on-el10); otherwise visibly Auto |
| **HDR** | Served as Grading, unless the operator declares a Rec.2100 PQ desktop (`video.desktop_encoding`) | Real BT.2020/PQ from the compositor on GNOME/Mutter 50 or newer, same proof and limits as Grading |
| **No NVENC** | OpenH264, H.264 4:2:0 | OpenH264, H.264 4:2:0, up to 30 fps, outputs fitted to 1920×1200 |
| **Monitors** | One to four; NVIDIA heads discovered at start | One to four Mutter virtual outputs, Remote UI scale, Windowed resize; Speed, Grading and HDR on one output only |
| **Cursor** | The Mac's own cursor; the host sends shape changes (XFixes) | The Mac's own cursor drawing the host's cursor images, where qualification proves Mutter's cursor metadata; never a second cursor in the video |
| **Pen (Tablet support)** | Virtual tablet: pressure, tilt, eraser, barrel buttons | Native desktop: a real pen with pressure and tilt in Wayland and Xwayland applications. Headless desktop: acts as a mouse until GNOME's EIS offers a stylus |
| **Native tablet (USB bridged)** | Yes, on a LAN | Refused |
| **Keyboard** | Keys | Keys, plus typed text from the Mac's input method within the session's keyboard layout |
| **Clipboard** | Both ways, text and PNG | Same, same policy |
| **Audio and microphone** | An Arcen sink in the user's audio server; every other sink muted for the session | A private PipeWire graph per session with no speaker in it; native PipeWire and PulseAudio applications both heard |
| **X11 applications** | Native | Through Xwayland, where qualification proves the X11 pointer |
| **Deskside privacy** | Yes | Not armed: a Wayland desktop never drives the physical console |
| **Host requirements** | Xorg, NVIDIA driver (NvFBC) | GNOME Shell/Mutter 40 or later, PipeWire, WirePlumber, a DRM render node (for NVIDIA, `nvidia-drm modeset=1`), a qualification record. Native desktops also need an NVIDIA GPU without display connectors, the in-tree `vkms` module and polkit 0.106 or later |

### What a session carries

| | |
|---|---|
| **Video** | Four complete presets: Auto, Speed, Grading, and HDR. They select separate capture/conversion pipelines rather than adding switches to one path. Source-built OpenH264 is the software floor where no supported hardware encoder exists. See [Streaming presets and independent pipelines](#streaming-presets-and-independent-pipelines). |
| **Multiple monitors** | One to four, negotiated as one topology, and **on by default on every host**. The Deck can match its own display layout. The Linux Pier discovers the GPU's NVIDIA heads when the service starts; the Windows Pier picks an eligible NVIDIA adapter at start (list any GPU you want kept for other work in `excluded_adapters`); the macOS Pier creates one virtual display per Deck display. **Each monitor is its own NVENC session** on Linux and Windows, and that is the real ceiling there: consumer GeForce cards limit how many encode sessions run at once, while RTX Pro, Quadro and GRID do not. Arcen never guesses this from the GPU model — it opens the whole planned encoder set and admits it only if every region meets the quality thresholds. With `allow_software_fallback`, monitors that miss out can drop to OpenH264 — but **4:4:4 monitors cannot**, since OpenH264 is 4:2:0 only. `advertise_enabled: false` turns multi-display off. |
| **Audio out** | 48 kHz stereo, Opus-compressed by default, or uncompressed PCM if you would rather spend bandwidth than CPU. By default the host's own speakers go silent for the session (`audio.local_playback`), so nobody beside the machine hears the remote user. On macOS that silence needs **System Audio Recording**, a separate approval from Screen Recording: the helper asks for it when it starts, and until it is given the host refuses a session and tells the Deck why rather than serving it audibly. On Linux an X11 session plays into its own Arcen sink while every other sink of the user's audio server is muted, for the whole session; a Wayland session's audio is a private graph with no speaker in it, so native PipeWire and PulseAudio applications are heard by the Deck and not in the room. A Linux host that cannot prove the silence refuses the session; `local_playback: audible` lets the host's speakers play as well. The Windows Pier does not implement `local_playback` yet. |
| **Microphone in** | **Linux hosts; macOS Pier preview.** Client microphone into the host session, Opus or fixed-rate PCM. Enabled in the host's default configuration, and still opt-in on the Deck every launch — consent is deliberately never restored from settings. Windows needs a signed driver Arcen does not yet ship (see [Where help is wanted](#where-help-is-wanted)). The macOS Pier publishes a virtual microphone and offers it only when that device is present; end-to-end capture is built but not yet qualified. |
| **Keyboard and pointer** | Absolute and relative motion, and negotiated cursor authority — the host draws the cursor, or the client does, but never both. Trackpad scrolling stays precise end to end (phased deltas on macOS, high-resolution wheel on Linux, fractional and horizontal wheel on Windows). Trackpad gestures — magnify, smart zoom, swipe — are negotiated separately and injected where the host OS allows it (the macOS Pier today). |
| **Typed text** | **Linux Wayland hosts.** What the Mac's input method composes (accents and dead keys, Japanese, Chinese, Korean, emoji) and keys the remote layout would type differently arrive as the characters you typed; everything else, shortcuts and games included, still goes as keys (Settings → Input → Typing). The host types only what its session's keyboard layout can type, proven against the compositor's keymap, and tells the Deck which characters it could not type and why. Characters outside the session layout need a compositor with libei `ei_text` (GNOME's is not released yet), so on GNOME 49 they are refused visibly. Xorg, Windows and macOS hosts keep key positions for now. |
| **Pen and tablet** | Any HID pen tablet, whoever made it: a device counts as a tablet because its HID report descriptor declares a pen, never because of its vendor id. Three modes, chosen per connection, because the choice is really about the network — it decides where the pen is interpreted.<br><br>**Tablet support** *(default, any distance)* — the Mac's own tablet driver reads the pen and Arcen sends finished pen events. Nothing waits for a reply and the host needs no tablet driver. Pressure, tilt, rotation, eraser, proximity and barrel buttons all work; finger touch and the tablet's own buttons stay on the Mac, which keeps working in Mac applications.<br><br>**Native tablet (USB bridged)** *(LAN only, Linux X11 hosts; see below for Windows, macOS and Linux Wayland)* — a privileged helper takes the device from macOS and forwards raw USB, so the host's own driver for that tablet claims it. The whole device works, including finger touch and the tablet's buttons. Every sample makes a full round trip, and Mac applications lose the tablet until you disconnect.<br><br>**Mouse compatibility only** — no redirection; the pen acts as a mouse.<br><br>On Linux Wayland hosts Tablet support gives a real pen (pressure and tilt) on a desktop that runs natively on its own seat (`wayland.session_mode`, one per host); on a headless desktop the pen acts as a mouse until GNOME's EIS offers a stylus. Native tablet is refused on every Wayland desktop. See [How Wayland support works](#how-wayland-support-works). On Windows and macOS the native-tablet mode is refused rather than quietly downgraded, so a Deck that asks for it is told it is unavailable and keeps ordinary mouse control. On macOS the reason is measured: presenting a bridged device needs `IOHIDUserDevice`, which returns nothing without the `com.apple.developer.hid.virtual.device` entitlement — as an ordinary user and as root alike. Apple has now granted it, but nothing on the host consumes bridged tablet traffic yet, so the mode stays refused until that importer exists. |
| **Clipboard** | Text and images, both directions, or restricted to one, or off. The host decides; the client cannot override it. |
| **Timezone** | The session follows the client's timezone, so timestamps read the way you expect. Linux sets `TZ` in the remote process tree; Windows temporarily changes the system timezone; the macOS Pier sets `TZ` in the served GUI session, so apps launched during the session see the Deck zone while the menu bar clock and already-running apps stay on the Mac's zone. |
| **Login banner** | Off by default. A host can require the user to read and accept an operator-written notice *before* the Deck collects any credentials, with the exact text recorded. Useful where a legal warning is mandatory. |
| **Reconnection** | A dropped connection resumes the same session for up to two hours. The Deck reattaches with a signed grant instead of asking for the password again. |
| **Bitrate** | Follows the network path through one shared controller on every host, and QUIC uses BBR congestion control, so the random loss of Wi-Fi, cellular and VPN links is not mistaken for congestion. When an encoder cannot sustain a preset's frame rate the session is admitted at the best proven rate and says so, instead of being refused. |
| **Deskside privacy** | Linux and Windows hosts. Off by default. When enabled, the physical screen is blanked and its keyboard and mouse are disabled for the duration of a remote session, so nobody standing at the machine can watch or interfere. Input and display are locked together — you cannot get one without the other. |

### Streaming presets and independent pipelines

Arcen does not take one 8-bit capture path and relabel it for every mode. The
presets choose complete contracts, and each host selects a capture provider
that can actually supply that contract.

| Preset | Requested stream | Windows Pier | Linux Pier (Xorg) | macOS Pier (preview) | Deck presentation |
|---|---|---|---|---|---|
| **Auto** | 30 fps, adaptive 4:2:0 8-bit | DDA when it proves real frames, otherwise WGC BGRA8 | NvFBC → CUDA → NVENC; device-to-device fast path | ScreenCaptureKit → VideoToolbox; hand-qualified as `420v`/`Yuv420` on one lab Mac | Ordinary SDR VideoToolbox path |
| **Speed** | 60 fps, adaptive 4:2:0 8-bit | Same 8-bit capture path at the higher cadence | Same NvFBC fast path at the higher cadence | Same macOS 8-bit path at the higher cadence; not a release qualification | Ordinary SDR VideoToolbox path |
| **Grading** | 30 fps, HEVC 4:4:4 10-bit full-range BT.709 | WGC FP16 scRGB → SDR OETF/matrix → NVENC P16 | Depth-30 Xorg → XShm RGB10 → shared conversion → CUDA upload → NVENC P16 | ScreenCaptureKit 10-bit 4:4:4 → VideoToolbox; hand-qualified as `x444`/`Yuv444` on one lab Mac | Native `xf44` → dedicated `RGB10A2Unorm` Metal layer, SDR |
| **HDR** | 30 fps, HEVC 4:4:4 10-bit full-range BT.2020/PQ | HDR EDID/topology → exact-target Windows HDR state → WGC FP16 scRGB → absolute PQ/BT.2020 → NVENC P16 | Declared `rec2100-pq` desktop: depth-30 XShm → BT.2020 NCL → NVENC PQ. Otherwise resolves to the Grading pipeline and reports matrix/primaries/transfer degradation | Served when the display being captured reports headroom above SDR white; the virtual display made for an HDR Deck display does. Otherwise the session is served as Grading and says so. | Same 10-bit Metal layer, with PQ colour space and EDR only when the host returns PQ |

The separation is deliberate:

- the Linux 8-bit path keeps its no-host-copy advantage and never pays the
  XShm/CPU/upload cost;
- ten-bit SDR is not treated as HDR;
- Windows Grading uses a genuine FP16 source without changing the desktop into
  HDR mode;
- Windows HDR starts only after the requested display itself proves HDR; and
- a fallback is reported to the Deck instead of being hidden inside a deeper
  container.

The detailed colour contract and measured evidence are in
[`docs/architecture/color-fidelity.md`](docs/architecture/color-fidelity.md).

Native tablet needs a host that can present the tablet on a virtual USB
controller so the host's own driver can claim it. **Linux hosts can. Windows
hosts cannot yet** — that needs a signed driver Arcen does not ship, and it is
planned rather than abandoned; see
[Where help is wanted](#where-help-is-wanted). Tablet support is unaffected on
Windows and needs nothing installed there.

On macOS the privileged part of Native tablet is a small root daemon registered
through `SMAppService`, not the Deck: Arcen Deck itself never runs as root.
Installing it is one approval in Login Items. See
[ADR 0011](docs/adr/0011-macos-privileged-usb-helper.md).

Keyboard, pointer and scroll are synthesised by the hosts: they arrive as
native events the operating system generates on Arcen's behalf. On Linux this
goes through the kernel's own input layer; on Windows through the injection API;
on the macOS Pier, pointer/scroll injection works without Accessibility and
the keyboard needs the Accessibility approval the helper asks for when it
starts. At the macOS login window, where synthetic events do not reach, the
Pier types through a virtual HID keyboard and pointer.

**Not supported:** webcam redirection of any kind, and USB passthrough for
anything other than the one tablet class above. Neither is a small gap —
[`docs/architecture/macos-peripheral-access.md`](docs/architecture/macos-peripheral-access.md)
sets out what a webcam would take and which of the two routes to try first.

Arcen expects a **trusted network**. It is not hardened against the open
internet. Put it behind a VPN or a network you control.

---

## Install

### The host (Pier)

Download the installer for your platform from
[Releases](https://github.com/Aanerud/arcen_public/releases) and run it with
administrative rights. One file. No runtime to install first, no package
repository to add, no service file to write.

**Linux** — needs systemd, `openssl`, and root:

```sh
sudo ./install-arcen-pier
```

The Linux installer chooses the desktop from what the machine has, never from
the distribution's name, and prints what it found and why (also appended to
`/var/log/arcen/install.log`). An installed Xorg server keeps the dedicated
X11 desktop, exactly as before; an existing install never moves. Without
Xorg, a headless GNOME Shell 40+ with PipeWire, WirePlumber, `dbus-daemon`
and a DRM render node is qualified for Wayland with a short real session
(`arcen-pier wayland qualify --write`), and the service starts only if that
passes; otherwise the Pier stays installed but stopped, with what failed and
what to run next. A host with neither Xorg nor DRM is refused before
anything is written, naming the driver to install (for NVIDIA, `nvidia-drm`
with `modeset=1`). The installer never installs packages or changes driver,
kernel, device-permission or boot configuration, or the SELinux mode; where
SELinux is enabled it loads only Arcen's own one-rule policy module (and
unloads it on uninstall), and where polkit is installed it adds Arcen's
rule that keeps remote desktops remote (see
[How Wayland support works](#how-wayland-support-works)). On a host
whose NVIDIA GPU has no display connectors, qualification also tries a
native desktop, which gives the pen pressure and tilt; the installer
explains the display-manager hazard that comes with it and disables the
display manager only with `--disable-gdm`. `--check` prints the detection
and decision and writes nothing.

**Windows** — open **PowerShell as Administrator**, then run the installer from
the folder you downloaded it to:

```powershell
.\install-arcen-pier.exe
```

**macOS** — a preview package, `ArcenPier-<version>.pkg`, signed and
notarised. Open it in Installer.app on the Mac it installs; the command-line
`installer` is refused, because the privacy approvals that follow need a
person at that Mac. It is a preview, not yet a supported host: the cold-boot
login window and multi-display on physical hardware are unqualified, and it
has not completed Release/Security review.

The macOS Pier is deliberately **more than one app**, and the installer says so
before it changes anything:

- **Arcen Pier**, the network service. It starts at boot, listens on UDP 18444,
  holds the host key and checks passwords. It cannot see or control any screen,
  and it runs as `_arcen`, a hidden account with no password and no shell. It
  refuses to run as root.
- **Arcen Agent Helper**, in `/Library/PrivilegedHelperTools`. macOS starts one
  in each signed-in session, as that person, to capture and to type. It has no
  port and cannot read the key.

They cannot be one program. macOS allows capture and input only inside a
signed-in session, while a host must answer when nobody is signed in and must
not fight over its port when two people are. Creating `_arcen` is why macOS asks
once, on first install, *"Installer would like to administer your computer"*.
Nothing else about any account changes. Each person who will be served then
approves Screen & System Audio Recording and Accessibility for Arcen Agent
Helper. The full reasoning, what each part can and cannot do, and the one open
gap (the login-window agent runs as root) are in
[`docs/security/macos-pier-process-model.md`](docs/security/macos-pier-process-model.md).

The installer creates the directories, generates a TLS certificate, registers
and starts the service, and opens **UDP 18444** on a firewall it recognises.
It succeeds only when the Pier is proven running. Every host gets the same
configuration with everything on — audio, microphone, clipboard, time zone,
10-bit video, multi-display — and the file exists for turning things off. An
upgrade keeps the existing configuration and the host's certificate, so
remembered Decks keep trusting it.

On Windows, **reboot afterwards**. Windows reads the list of credential
providers only when the login screen starts, so the provider Arcen just
installed stays invisible until the machine restarts. Skip the reboot and your
first remote sign-in fails, asking you to install something that is already
installed.

Useful flags on Linux and Windows: `--dry-run` shows what would happen and
changes nothing; `--uninstall` and `--uninstall --purge` are described under
[Uninstall](#uninstall).

### The client (Deck)

Download `Arcen-Deck-<version>-macOS.zip` for an Apple-silicon Mac, or
`Arcen-Deck-Intel-<version>-macOS.zip` for an Intel Mac, unzip it, and drag the
app (`Arcen Deck` or `Arcen Deck Intel`) to `/Applications`. Both are signed and
notarised, so they open normally, and both need macOS 12 or later. Install only
the one that matches your Mac: the two share one bundle identifier, so they
share settings and privacy permissions and are not meant to sit side by side.

### Uninstall

Every host uninstaller keeps the host's identity by default — its TLS
certificate and configuration — so a reinstall presents the same certificate
and Decks that remember the host keep trusting it. Add `--purge` to remove that
too; it is a deliberate choice, because Decks must then pin the new
certificate.

**Linux Pier** — with the installer you installed from:

```sh
sudo ./install-arcen-pier --uninstall          # keeps /etc/arcen and the logs
sudo ./install-arcen-pier --uninstall --purge  # removes them as well
```

It stops and disables the service and removes the binary, the systemd unit,
the documentation and the log rotation rule, Arcen's SELinux policy module
where one was loaded, Arcen's polkit rule, the Wayland qualification
record and any leftover temporary qualification account, and the UDP 18444
firewall opening if the installer was the one that opened it. A display
manager that `--disable-gdm` turned off is put back. `--purge` first saves a copy of
`/etc/arcen/pier.json` as `/etc/arcen-pier.json.purged-<epoch>`.

**Windows Pier** — in **PowerShell as Administrator**, from the folder with the
installer:

```powershell
.\install-arcen-pier.exe --uninstall          # keeps C:\ProgramData\Arcen
.\install-arcen-pier.exe --uninstall --purge  # removes it as well
```

It removes the service, the binaries, the credential provider registration, the
event source and the installer's firewall rule. `--purge` first saves
`pier.json` as `C:\ProgramData\arcen-pier.json.purged-<epoch>`. If it reports
that the credential provider is still loaded by the sign-in screen, reboot and
confirm the directory is gone.

**macOS Pier** — the uninstaller ships inside the app, so it cannot go missing:

```sh
sudo "/Applications/Arcen Pier.app/Contents/Resources/uninstall.sh"          # keeps identity, config and logs
sudo "/Applications/Arcen Pier.app/Contents/Resources/uninstall.sh" --purge  # removes them as well
```

It stops the service and the helper in every signed-in session, removes the
launchd definitions, the PAM policy, the log rotation rule, the virtual
microphone and both applications, then checks that nothing is left running.
Without `--purge` it keeps `/Library/Application Support/Arcen` (identity and
configuration), `/Library/Logs/Arcen/Pier`, the hidden `_arcen` account and the
Screen Recording and Accessibility approvals; `--purge` removes all of them.

**Deck** — quit it and drag `Arcen Deck` (or `Arcen Deck Intel`) from
`/Applications` to the Bin. Saved hosts, pinned certificates and settings live in
`~/Library/Application Support/Arcen` and the login keychain; delete them only
if you want the Deck to forget every host. If you used Native tablet, check
that the Arcen USB helper no longer appears under **System Settings → General →
Login Items & Extensions**.

---

## Connect

1. Open Arcen Deck.
2. Enter the Pier's address. The port is 18444.
3. The first connection shows the host's certificate fingerprint. Compare it
   with the Pier and choose **trust and remember**.
4. Sign in with an ordinary account on the host machine.

Arcen does not keep its own list of users. It asks the operating system — PAM on
Linux, the normal login path on Windows — so the account you already have is the
account you use.

### Test without the interface

The Deck can connect from a terminal, which is useful for checking a host:

```sh
"/Applications/Arcen Deck.app/Contents/MacOS/arcen-deck" \
  --connect <host> 18444 \
  --credentials-stdin \
  --frames 120 --timeout-secs 60
```

It reads the username and password from standard input, two lines, so neither
appears in your shell history. Progress goes to
`~/Library/Logs/Arcen/`. Look for `stream healthy` and a rising
`frames_decoded`.

### If it feels slow, ask for sixty

The Deck's **Standard** performance mode requests 30 fps. **High** requests 60,
and on a macOS Pier that is worth far more than the name suggests — measured
against the same host over the same link, changing only the requested rate:

| | 30 fps | 60 fps |
| --- | --- | --- |
| frames actually delivered | 29 | 57 |
| age of a frame on arrival | 27 ms | 6 ms |
| time a change waits to be captured | ~17 ms | ~8.7 ms |

About 59 ms of pipeline becomes 27 ms. Roughly two thirds of that is not the
encoder at all: at 60 fps the host photographs the screen twice as often, so a
change waits half as long to be seen in the first place.

It costs bandwidth — about 7.4 Mbps against 4.6 — and on anything faster than a
domestic uplink that is a trade worth making. From the terminal, add
`--max-fps 60`.

---

## How it works

A **Pier** captures a desktop, encodes it, and sends it. A **Deck** receives it,
decodes it, and sends your keyboard and mouse back. Everything else is detail.

**One connection, one protocol.** Arcen speaks QUIC over UDP 18444 and nothing
else. There is no TCP fallback and no unencrypted mode. QUIC carries TLS 1.3
itself, so the encryption is part of the transport rather than bolted on.

**The client picks a result, not a codec.** The Deck asks for Auto, Speed,
Grading, or HDR and reports what it can decode. The Pier chooses the concrete
capture and encoder path, because only the Pier knows what its display and GPU
can supply. If it cannot serve the requested result, it reports the lower
fidelity plan rather than pretending.

**How frames leave the host.** Capture is platform-specific and
contract-specific. The 8-bit, ten-bit SDR, HDR, and software paths are separate
pipelines with separate admission rules; they are not one screenshot loop with
different labels.

On **Linux**, Auto and Speed use **NvFBC → CUDA → NVENC**. The frame stays on
the GPU. Grading cannot use that path because public NvFBC has no ten-bit
format, so it uses **depth-30 Xorg → MIT-SHM → packed RGB10 conversion → CUDA
upload → NVENC**. X11 has one depth per server, and at depth 30 browsers lose
the GPU, so a Linux desktop is ten-bit only when the session that creates it is
(`video.x11_desktop_depth`). Xorg has no HDR composition contract; an HDR request therefore
uses that same genuine ten-bit SDR pipeline with an explicit downgrade. A
machine without usable NVENC takes a third path: checked X11 capture and shared
conversion into source-built OpenH264. A headless Wayland session without
usable NVENC takes the same encoder through its own composition: the
compositor's shared-memory frames, Keel's hash, and the one OpenH264 session
both providers share.

On **Windows**, Auto and Speed use the 8-bit D3D11 path: Desktop Duplication
only when it proves real image delivery, otherwise WGC. Grading requires a
different WGC pool, `R16G16B16A16Float`, and converts linear scRGB into ten-bit
BT.709. HDR adds another upstream contract: Arcen first provisions the final
HDR EDID/topology, enables HDR on the exact bound display target, verifies
PQ/BT.2020, and only then converts the FP16 scRGB source into HDR10. WGC refusing
the FP16 pool is a hard failure; Arcen never falls back to BGRA8 while claiming
ten-bit.

**What 10-bit means depends on the source.** Linux Grading reads genuine
depth-30 Xorg codes. Windows Grading and HDR read FP16 scRGB. An 8-bit source
encoded into ten bits can still avoid RGB↔YCbCr rounding loss, but Arcen does
not confuse that mathematical benefit with a genuinely wide capture source,
and it never treats bit depth alone as proof of HDR.

**Codec choice follows the hardware — at both ends.** The Pier can only send
what the Deck can actually decode, so the Deck measures its own capability at
startup and reports it in the handshake.

On macOS that means VideoToolbox, and the answer differs by machine. Every Apple-silicon
Mac decodes H.264 and HEVC in hardware; an Intel Mac whose GPU has no decoder
for a stream decodes it in software, and reports only what a real decode
session accepted. **AV1 hardware decode arrived with the M3**. An M1, an M2, or
any Intel Mac reports no AV1 (the Intel app never offers it), and the Pier
picks HEVC instead. Nothing degrades silently; the negotiation simply lands
somewhere else.

| Host has | Deck can decode | Arcen uses |
|---|---|---|
| Modern NVIDIA GPU | AV1 (M3 or later) | Hardware AV1, encoded and decoded in hardware |
| Modern NVIDIA GPU | No AV1 (M1, M2, Intel) | Hardware HEVC |
| Older NVIDIA GPU | HEVC | Hardware HEVC, then H.264 |
| Any NVENC GPU | HEVC 4:4:4 10-bit | Hardware HEVC 4:4:4 10-bit, for colour-critical work |
| No GPU at all | H.264 | Source-built OpenH264, decoded in hardware on the Deck |

Note the last row: software encoding on the host still gets **hardware decoding
on the client**, because the output is ordinary H.264. The cost lands on the
host's CPU, not on the laptop in front of you.

The Deck does not guess. It builds a real decode session for each format and
reports only what succeeded — separately for 4:4:4, 10-bit, 12-bit and colour
range. The code is blunt about why: claiming a capability the Deck has not
demonstrated is worse than admitting it, because the host believes the claim and
sends a stream the Deck then cannot decode.

### Keel: send what changed, and nothing else

Keel ([`shared/keel`](shared/keel)) is the part of every Pier that decides
whether a frame is worth sending. It works at two levels, and they are easy to
confuse.

**Frame level — on for every hardware pipeline.** An encoder submits a frame
when the desktop changed, when the Deck asked for a keyframe, once more to
flush NVENC's one-deep output queue, and otherwise once a second as a
keepalive. A still desktop therefore costs a fraction of a megabit, not a full
stream: a Windows RTX session measured 150–400 kbit/s while idle and 5–9 Mbit/s
while a video played. Each Pier gets the change signal from its own operating
system:

| Pier | Change signal |
|---|---|
| Linux, Auto/Speed (NvFBC) | NvFBC's new-frame flag. The CUDA capture interface has no per-block damage, so this path has frame-level gating only. |
| Linux, Grading/HDR (XShm) | X Damage events |
| Linux, Wayland | The compositor's per-buffer damage regions (GNOME 49 and later), which also feed the QP map; GNOME 40 sends none, so every frame there is full damage |
| Windows, every preset | Desktop Duplication and WGC deliver a frame only when the desktop changed; their dirty/move rectangles and dirty regions feed the QP map |
| macOS | ScreenCaptureKit dirty rectangles |
| Software fallback | Keel's own 16×16 block hash, which also skips unchanged rows during colour conversion |

**Block level — an encoder hint, off by default.** With `video.qp_map` set
(one value, or per preset: `{"auto": "on", "speed": "on"}`), Keel turns the
same damage into an NVENC QP map so changed blocks get the bits. It engages
only where damage is free: OS rectangles on Windows, X Damage on Linux
Grading/HDR, compositor damage regions on Wayland. The Linux NvFBC
fast path refuses it rather than add a full-frame
GPU readback. Every encoder logs
`QP map policy=… engaged=… damage_source=…`, so whether it ran is never a
guess. See [`docs/architecture/qp-maps.md`](docs/architecture/qp-maps.md).

Modern codecs already code an unchanged block almost for free, so Keel's gain
is mostly the frames it never encodes. What it does not do yet is *refine*: a
static region that arrived during motion stays at its first quality until it
changes. Spending spare bits to sharpen blocks once they stop changing — the
"build to lossless" behaviour other remote-desktop products use — is open work.

### How frames reach the glass on the Deck

Both the 8-bit and the 10-bit streams normally present decoded `IOSurface`s
through a dedicated `CAMetalLayer`, with no CPU copy; the ordinary window
surface remains only as a fallback that the Deck reports when it is used. A shared display-refresh pacer
([`shared/media/src/presentation.rs`](shared/media/src/presentation.rs)),
driven by the window's `CVDisplayLink`, shows at most one new picture per
refresh from a queue of at most three, dropping the oldest on overflow and
anything stale after a stall. Two lessons from real sessions are built in:

- **Never take a frame you cannot show.** When every drawable is still in
  flight, the refresh waits and the frame stays queued for the next one.
  Taking it and then finding no drawable silently threw away about one frame
  in four on a 60 Hz panel.
- **A network stall arrives as a burst.** A 250–300 ms Wi-Fi or VPN stall
  releases every frame produced meanwhile at once. The Deck's inbox holds about
  half a second, so the burst is decoded and the newest frame shown; a smaller
  inbox discarded the chain, asked for a keyframe, and the large keyframe made
  the next burst bigger.

The Deck logs `dedicated presenter telemetry` every second — received,
submitted, confirmed, drops by cause, slot waits, and refresh intervals — and
the host's health check compares decoded against presented frames, so a
presentation problem shows up even when nobody notices it on screen.

### How Wayland support works

**The Pier picks the desktop from facts, then proves it.** At start
(`capture.provider`, default `auto`) an installed Xorg server means X11.
Without one, Wayland needs a GNOME Shell/Mutter that can run headless with
virtual monitors, systemd-logind, `dbus-daemon`, PipeWire, WirePlumber, a
DRM render node — and a qualification record. `arcen-pier wayland qualify
--write`, which the installer runs, signs a temporary account into a real
desktop and measures what this host can actually do: whether GPU import of
the compositor's DMA-BUFs is pixel-exact, which display sizes hold Speed's
rate, whether the compositor's cursor metadata, Xwayland and the precision
needed for Grading and HDR are there, and whether a native desktop works.
The service re-qualifies when the Pier, GNOME, PipeWire or the GPU driver
changes. A capability the record does not prove is never assumed: the
session is served at the level that was proven and the Deck shows why. The
lab hosts' evidence is in
[`docs/architecture/wayland-qualification/`](docs/architecture/wayland-qualification).

**Each sign-in gets a whole GNOME session of its own** — a logind session, a
private D-Bus bus, PipeWire and WirePlumber, then GNOME Shell with the
keyring unlocked by the sign-in password, the settings daemons and the
user's autostart programs. Arcen drives Mutter's own ScreenCast and
RemoteDesktop D-Bus APIs on that private bus: video arrives as PipeWire
buffers in the same capenc that serves X11, input goes in through
RemoteDesktop and its EIS socket, and so does the clipboard. There is no
portal and no screen-sharing prompt, and the compositor is the
distribution's own (or, for Grading and HDR where the distribution ships
GNOME 49 or older, an optional newer GNOME installed beside it).

**Two ways to run that session** (`wayland.session_mode`):

- **Headless** — GNOME Shell with `--headless` and virtual monitors, in a
  seatless session. It needs no seat and changes nothing on the host. Its
  limit is the pen: headless Mutter turns libinput off
  ([`META_SEAT_NATIVE_FLAG_NO_LIBINPUT`](https://gitlab.gnome.org/GNOME/mutter/-/raw/main/src/backends/native/meta-backend-native.c)),
  so it cannot see a tablet, and the remote-input route meant for one — a
  stylus over libei — is not there yet: libei's `ei_stylus` is merged
  ([libei!400](https://gitlab.freedesktop.org/libinput/libei/-/merge_requests/400),
  [announcement](https://who-t.blogspot.com/2026/07/libei-and-graphics-tablets-stylus.html))
  but not in a release, and Mutter's EIS server offers no stylus. The Pier
  probes for one at every sign-in and reports what it found; until a
  compositor offers one, the pen acts as a mouse. (Enabling libinput in headless
  Mutter, [mutter!5224](https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/5224),
  is still open; Arcen keeps a lab-only route for it, off by default.)
- **Native** — the same stock compositor, started as a display server on a
  logind seat of the desktop's own (`seat-arcen-pen-<id>`). Native Mutter
  runs libinput, so the Deck's pen reaches applications as a real tablet
  (`zwp_tablet_v2`) with pressure and tilt. Native mode needs a display head
  with a connector, which a vGPU does not have, so the seat holds three
  devices: the NVIDIA card node, which renders; the kernel's own virtual KMS
  driver, `vkms`, as the head; and the desktop's virtual tablet. The Pier
  puts them there with volatile udev rules under `/run`, logind hands them to
  the compositor and revokes them when the session ends, and everything is
  undone at the desktop's end, at Pier stop and at the next start. One
  native desktop runs per host; the default, `auto`, uses it when
  qualification proved it and otherwise serves a headless desktop and logs
  why. A display-manager restart would put a login screen on that seat, so
  the installer explains it and offers `--disable-gdm`.

**A seat makes polkit treat a session as local**, ignoring that logind
recorded it as remote. That is true of every X11 desktop too, and before
0.17.0 it gave remote X11 users the console's rights. The installer now
writes one rule, `/etc/polkit-1/rules.d/05-arcen-remote-session.rules`,
that gives remote, Arcen-launched and Arcen-seat sessions polkit's remote
answers, and the Pier asks polkit (`pkcheck`) about each seated desktop
before handing it over, refusing it on any grant.

Details: [the Wayland provider](docs/architecture/linux-wayland-provider.md)
(how each part works, with lab evidence),
[the administration guide](docs/operations/pier-administration.md#which-desktop-a-linux-pier-serves)
(choosing, qualifying, native desktops, polkit, the optional GNOME prefix),
[ADR 0014](docs/adr/0014-quic-lanes-and-capture-contracts.md) (capture
contracts) and
[ADR 0015](docs/adr/0015-native-wayland-desktops-on-their-own-seat.md)
(native desktops).

### Three ways to run a Pier

Hardware encoding is the fast path, but it is not the only intended one.

**On a workstation with an NVIDIA GPU.** The full range, including HEVC 4:4:4
10-bit for colour-critical work. Both the Linux and Windows Piers do this.

**On a virtual machine.** Proxmox, VMware, and similar hypervisors usually give
a guest no encoder at all. Arcen uses source-built OpenH264. This is a
first-class way to run it rather than a consolation prize: it makes Arcen a
practical alternative to RDP or VNC on ordinary server hardware, with the same
QUIC and TLS 1.3 as everywhere else. You give up 4:4:4 and 10-bit, not
encryption and not the protocol.

**On a headless host.** Same as above, provided something gives the machine a
display to capture — a hypervisor's virtual adapter, or an indirect display
driver. Arcen ships no display driver of its own. On supported NVIDIA Windows
hosts it can instead provision unused native display IDs with session-scoped
EDIDs, including the HDR sink contract, and restores them when the lease ends.
That NVIDIA provisioning path currently requires the Windows account being
signed in to belong to the local Administrators group.

The encryption does not change between these. A software-encoded session on a
Proxmox guest is protected exactly as a 4:4:4 workstation session is.

**Only one session at a time.** A Pier drives one physical desktop. Two sessions
would fight over the same screen, the same mouse, and the same encoder, so the
Pier admits one and refuses the rest. If your connection drops, it holds your
session for up to two hours so you can resume where you left off.

---

## Security

Most remote-desktop projects gloss over this. Here it is plainly.

**How the Deck knows it reached the right Pier.** A Pier generates its own
certificate at install time, so there is no public authority to vouch for it.
The Deck therefore supports five modes:

| Mode | What it does |
|---|---|
| **System CA** | Ordinary certificate checking. Choose *High* in Advanced settings. It correctly refuses a self-signed Pier. |
| **Private CA** | Checks against a certificate authority you supply. The right choice for an organisation with its own. Command line only today — `--ca-bundle` — and a contribution adding it to the settings pane would be welcome. |
| **Trust on first use** | Shows you the fingerprint and asks. **The default**, because a freshly installed Pier is self-signed and this is the only mode that can connect to one without preparation. |
| **Pinned** | What trust on first use becomes once you remember a host. |
| **Insecure** | Development only. Requires *both* a setting *and* the `ARCEN_ACCEPT_INSECURE` environment variable, so one mistake cannot disable checking. A banner stays on screen for as long as it is active. |

**The first connection asks you.** The Deck shows the certificate fingerprint,
the public-key fingerprint, the validity dates, and the address that presented
them, then offers *cancel*, *trust once*, or *trust and remember*.

*Trust once* lasts until you quit. *Trust and remember* writes the host's
public-key fingerprint to `trusted_pins.json` and pins that exact identity to
that host and port — not to the saved connection, so quick-connecting to the
same address is protected too.

**A remembered host that changes its identity does not ask again — it fails.**
This is the point of the whole ceremony. Once a fingerprint is recorded, a
different one is a hard error with no "trust anyway" button, because an
impostor's certificate would otherwise raise exactly the same friendly dialog
the real host raised the first time. If the host really was reinstalled, you
can confirm that you checked with its administrator and choose *Forget and
Verify*. The Deck drops the old fingerprint, reconnects at once and shows you
the new one to compare before anything is trusted.

**Pins are compared in constant time**, so an attacker cannot learn a pin by
measuring how long a rejection takes. A pin changes only when you change it.

**Authentication belongs to the operating system.** PAM on Linux; on Windows, a
credential provider that takes part in the real login. Passwords are held in
memory that is wiped on release and never written to a log.

**There is no lockout in Arcen.** Repeated password attempts are passed to the
operating system, so throttling is whatever `pam_faillock` or your Windows
account-lockout policy already does. If you expose a Pier to a network you do
not control, configure that policy — Arcen will not do it for you.

### Using your own certificate

The installer generates a self-signed certificate so a fresh Pier works
immediately. That is why the Deck asks you to confirm a fingerprint: nothing
else vouches for it.

Give the Pier a certificate from a real authority and that step disappears. The
Deck's default mode already trusts the system authorities, so the connection
simply succeeds, the same way a browser does not interrogate you about a normal
website.

Point the Pier at your own PEM certificate and private key:

```jsonc
// /etc/arcen/pier.json   (Linux)
// C:\ProgramData\Arcen\pier.json   (Windows)
"tls": {
  "mode": "pem",
  "cert": "/etc/ssl/certs/pier.example.com.crt",
  "key":  "/etc/ssl/private/pier.example.com.key",
  "minimum_version": "TLS1.3",
  "expiry_warning_days": 30,
  "expected_sans": ["pier.example.com"]
}
```

Relative paths resolve inside the Arcen configuration directory; absolute paths
are used as given. Restart the service afterwards. `mode` accepts `pem` only —
there is no PKCS#12 or system-store option.

`expected_sans` is worth setting. The Pier validates its own certificate against
the names listed and **refuses to start** if the certificate does not cover
them. That fails closed on the common mistake of installing a certificate for
the wrong hostname, rather than serving a certificate every Deck will reject.
`expiry_warning_days` warns before the certificate lapses.

Then match the Deck to your situation:

| Your certificate | Deck mode | What the user sees |
|---|---|---|
| From a public authority | System CA — choose *High* | Connects. No prompt. |
| From your organisation's own authority | Private CA — `--ca-bundle` on the command line | Connects. No prompt. |
| Self-signed, as installed | Trust on first use — the default | Fingerprint prompt once, then remembered |

If you are running more than a handful of Piers, the middle row is the one
worth the effort. Issue them from your internal authority, hand the Deck your CA
bundle once, and no one has to compare fingerprints again.

A certificate covering extra names is easiest to get right at install time:

```sh
sudo ./install-arcen-pier --extra-san pier.example.com --extra-san 203.0.113.10
```

Add `--force` to replace a certificate that already exists.

To report a vulnerability, see [SECURITY.md](SECURITY.md). Please use private
disclosure.

---

## Architecture

Arcen is one rule repeated: **put it in `shared/` unless the operating system
forbids it.**

Eleven shared crates hold the logic. They are pure Rust with no operating-system
calls, which means they compile everywhere, they are tested everywhere, and a
new platform inherits them for free.

| Crate | Holds |
|---|---|
| [`protocol`](shared/protocol) | The wire format. The single source of truth for what a Pier and a Deck say to each other. |
| [`transport`](shared/transport) | QUIC and TLS: certificate lifecycle, validation, pinning. |
| [`media`](shared/media) | Codec negotiation, colour, clipboard rules, video plane maths. |
| [`input`](shared/input) | Mouse, keyboard, and pen: motion, ordering, cursor authority. |
| [`outputs`](shared/outputs) | Monitor lifecycle and multi-display arrangement. |
| [`session`](shared/session) | Reconnection and session-restore decisions. |
| [`identity`](shared/identity) | Resume grants and acknowledgement evidence. |
| [`keel`](shared/keel) | Damage tracking: which 16×16 blocks changed. |
| [`telemetry`](shared/telemetry) | The event vocabulary. |
| [`observability`](shared/observability) | Structured logging that cannot block a video thread. |
| [`usb-bridge`](shared/usb-bridge) | USB passthrough policy and state. |

Two rules keep this honest, and CI enforces both:

1. **Shared crates never depend on a host, a client, or packaging.** The
   dependency arrow points one way.
2. **Shared crates stay light.** `arcen-transport` must not drag a QUIC runtime
   into a program that only wanted certificate handling, and `arcen-media` must
   not drag in a native codec build. CI proves this by inspecting the dependency
   tree, not by trusting the author.

Platform code does only what platforms must: capture pixels, talk to a GPU
encoder, inject input, authenticate a user, put a window on screen.

---

## Writing a client for another platform

This is the most useful contribution available, and it is smaller than it looks.

**Most of the Deck is already portable.** Of roughly 70,000 lines, about 11,600
touch macOS at all. The user interface uses `egui`, which already runs on
Windows and Linux. The audio pipeline, the frame queue, and the multi-monitor
logic contain no macOS calls whatsoever.

What you would actually replace:

| Part | Lines | Why it is platform-specific |
|---|---|---|
| `pipeline/video_decoder.rs` | 4,590 | Uses VideoToolbox. Windows would use Media Foundation or D3D11; Linux, VA-API. **This is the real work.** |
| `display/` | 3,362 | Reading monitor layout and scale factors. |
| `tablet/` | 2,337 | Pen pressure and tilt. |
| `hid/` | 1,312 | Device passthrough. |

What you would keep unchanged: every shared crate, the whole user interface, the
QUIC transport, the audio path, the frame queue, and the reconnection logic.

A sensible order:

1. Connect and authenticate, using `arcen-transport` and `arcen-protocol`.
2. Decode video with your platform's decoder and display it.
3. Send input through `arcen-input`.
4. Add audio, then multi-monitor, then devices.

Read [`clients/macos/ARCHITECTURE.md`](clients/macos/ARCHITECTURE.md) first. It
documents the decisions and the reasons, including the ones that were wrong the
first time.

---

## Where help is wanted

Ordered by how much they matter.

**A Linux or Windows Deck.** See above. The largest gap and the clearest path.

**Signed Windows drivers — microphone and USB.** Two features are missing on
Windows for exactly the same reason, and solving one solves most of the other.

- **Microphone input.** Works on Linux, which publishes a virtual source through
  PulseAudio and needs nothing installed. Windows has no user-mode way to create
  a recording device at all: an application can *use* audio devices but cannot
  register one, so a virtual microphone must be a kernel-mode driver publishing
  a `KSCATEGORY_CAPTURE` endpoint. Every product that does this ships such a
  driver, including Microsoft's own Remote Desktop. Arcen's `arcen-microphone`
  driver exists in the tree and is excluded from released installers because it
  is unsigned.
- **USB passthrough.** Linux can attach a physical USB device to a session.
  Windows cannot, for the same reason. The design is settled in
  [`docs/adr/0012-hard-usb-on-windows-hosts.md`](docs/adr/0012-hard-usb-on-windows-hosts.md).

Neither is blocked on code. Both need **driver signing**: an EV certificate plus
a Microsoft Partner Center account for attestation, and WHQL/HLK on top of that
for Windows Server. If you already have that pipeline, this is mostly plumbing,
and it is the single most valuable thing anyone could contribute to the Windows
host.

There may also be a cheaper route worth investigating first. Arcen already
adopts a third-party GPL kernel module rather than writing its own for the Linux
USB bridge — see `usb-vhci` in
[`legal/THIRD_PARTY_NOTICES.md`](legal/THIRD_PARTY_NOTICES.md). An
already-signed open-source virtual audio driver could do the same job here and
skip the certificate problem entirely. Anyone taking this on should check that
before paying for a signing pipeline: confirm the licence combines with
AGPL-3.0, and confirm whether its signature satisfies the Windows editions you
care about, since community code-signing is not the same as Microsoft WHQL
attestation.

**Finishing the macOS Pier.** It ships as a preview: a native Rust host built
on the same shared contracts as the others, with display creation, capture,
input, clipboard, audio and packaging kept as thin macOS adapters. The
logged-in path is used from a Deck every day. What is missing is listed in
the table at the top: deskside privacy needs only a macOS adapter, since its
wire messages, configuration and policy already live in `shared/`, and the
virtual microphone that now ships needs end-to-end qualification.
Multi-display creates a virtual display per Deck display, but has not been
qualified with physical monitors attached to the Mac. Serving
the macOS login screen needs a `LoginWindow` launch agent and virtual-HID input,
because `CGEvent` posting does not reach that screen. Apple has now granted the
virtual-HID entitlement, and both pieces are built. Apple still publishes no
contract that third-party code may capture the login window, and cold-boot
operation has not been qualified. That agent also runs as root, the one place the
Pier holds more privilege than it needs; narrowing it to the virtual keyboard
alone is open work, described in
[`docs/security/macos-pier-process-model.md`](docs/security/macos-pier-process-model.md).
FileVault's pre-boot disk-unlock screen is not supported, for a different and
simpler reason: macOS is not running yet.

**The gateway.** Today a Deck must reach a Pier directly, which in practice
means the same network or a VPN. The gateway would carry traffic between them
across the internet: one public port, many sessions multiplexed inside a single
QUIC connection, with federated identity so the host never sees a password.

It was designed and partly built, then cut before publication because shipping
an unfinished network boundary is worse than shipping none. The reasoning
survives in the decision records — see ADRs
[0002](docs/adr/0002-transport-evolution.md) and
[0003](docs/adr/0003-authentication-and-entitlement.md) — but the code does not.
Anyone picking it up starts from the design, not from a half-built service.

This one is honest about its economics. It is a large piece of work, and the
author writes Arcen with AI assistance that is billed by the token. The gateway
resumes when that bill is comfortably covered — or sooner, if someone else wants
to build it. Both routes are open, and the [coffee link](#if-it-is-useful-to-you)
is the first one.

**Smaller, self-contained pieces:**

- Webcam redirection. Nothing exists. The design question — capture through
  AVFoundation with ordinary consent, or pass the USB device through with a
  privileged helper — is laid out in
  [`docs/architecture/macos-peripheral-access.md`](docs/architecture/macos-peripheral-access.md),
  along with the measurement that decides it.
- A real pen on Linux Wayland: an EIS stylus (`ei_stylus`) in Mutter, or
  headless libinput (mutter!5224) upstream, and other compositors than GNOME's.
- Keel refinement: sharpen blocks once they stop changing, and a damage
  source for the Linux NvFBC fast path (the GL interface reports a change map
  that the CUDA interface does not).
- AMD and Intel hardware encoding. Only NVENC exists today.
- Better software encoding for hosts with no GPU.

**Latency, if you like measuring things.** A performance review traced a frame
from capture to glass and an input event from the tablet to the host. The
pipeline is already bounded and drop-aware in the right places — frame queues
discard prediction chains rather than block, NVENC runs with no B-frames and no
lookahead, the media worker is off the UI thread — so what is left is copies and
serialisation, ranked here by what a measurement would actually show at 4K60.

- **A bounded async decode window.** VideoToolbox is currently made synchronous
  — `wait_for_async_frames` immediately after every submit — which serialises the
  worker behind the decoder and turns any hiccup into a stall. Two frames in
  flight would overlap receive, decode and present without reordering.
- **GPU-side colour conversion on the host.** Windows maps BGRA8 or FP16 WGC
  staging and performs the negotiated conversion on the CPU. Linux Grading
  starts in host memory because XShm is the ten-bit source, converts there, then
  uploads once to CUDA. This is raw-frame traffic, not compressed, so it
  dominates the fidelity modes without affecting the separate NvFBC fast path.
- **Binary input encoding.** Every mouse, key and pen event becomes a
  `serde_json::Value` and travels on an unbounded channel. Fine for a mouse;
  questionable for a 1000 Hz pen, where the queue can also become a latency
  buffer rather than a backlog you drop.

### How to contribute

Fork, branch, open a pull request. There is no contributor licence agreement:
your work stays under the AGPL-3.0, like everything else here.

Two things asked of any change:

1. **Put logic in `shared/` if it can live there.** Code in `clients/` or
   `hosts/` should be code that genuinely cannot be written portably.
2. **Say what you actually built and ran.** CI does not build the platform
   crates (see below), so your word is the evidence. "Built on Linux, ran the
   Pier tests, did not try Windows" is a good pull request description. Silence
   is not.

Every directory has an `AGENTS.md` that states who owns it and how to validate
it. Read the one for the area you are changing.

---

## Build

The workspace does not build in one command, and that is deliberate: the client
is macOS-only, the Windows host is Windows-only, the Linux host is Linux-only.

**Anywhere** — the shared crates:

```sh
cargo test --locked -p arcen-identity -p arcen-input -p arcen-keel \
  -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol \
  -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge
cargo clippy --locked -p arcen-protocol -p arcen-transport -- -D warnings
python3 -m unittest scripts/test_validate_observability.py
```

**Each platform, on that platform:**

```sh
# macOS
cargo build --release -p arcen-deck-macos
packaging/macos/build-deck-app.sh              # dist/macos/Arcen Deck.app on Apple silicon,
                                               # dist/macos/Arcen Deck Intel.app on Intel
packaging/macos/build-deck-app.sh --arch x86_64   # the Intel app from an Apple-silicon Mac
# Each app targets one architecture and macOS 12; see packaging/macos/README.md.
# On an Intel Mac, Command Line Tools 14.2 suffice.

# Linux — needs libpam0g-dev and libpulse-dev
cargo build --locked --release -p arcen-pier-linux
ARCEN_PIER_BINARY=target/release/arcen-pier \
  cargo build --locked --release -p arcen-pier-linux-installer

# Windows, MSVC
hosts\windows\build.cmd
```

Before proposing a change, run the publication check. It refuses private
addresses, key material, vendor SDK payloads, and any crate that forgets to
declare its licence:

```sh
scripts/ci/check-publication-hygiene.sh
```

Releasing the macOS client requires signing and notarisation. The procedure is
in [`docs/operations/macos-signing.md`](docs/operations/macos-signing.md).

---

## Continuous integration

CI runs **manually**, on **Linux only**, and covers the shared crates, the strict
lint gate, and the publication check.

It does not build the platform crates. They need three different operating
systems, and a gate that cannot pass on the machine it runs on teaches nobody
anything. What runs here passes, so a green result means something. Platform
builds are the contributor's responsibility.

---

## Documentation

| Where | What |
|---|---|
| [`docs/architecture/`](docs/architecture) | How each part works and why |
| [`docs/adr/`](docs/adr) | Decisions, including withdrawn ones, with reasons |
| [`docs/operations/`](docs/operations) | Running and releasing |
| [`docs/security/`](docs/security) | Trust boundaries, [why the macOS Pier runs as separate processes](docs/security/macos-pier-process-model.md), and [the Linux Pier under SELinux](docs/security/linux-selinux.md) |
| `*/ARCHITECTURE.md` | Component detail, beside the code |
| `*/AGENTS.md` | Who owns a directory and how to validate it |

Decisions that were reversed are kept and marked withdrawn. A record that only
shows the wins teaches nothing.

---

## Origins

Arcen is original work. Existing remote-desktop and pixel-streaming systems
informed it — the shape of the problem, the vocabulary, the performance bar —
but no third-party source, SDK payload, or vendor implementation was copied into
it. See [`legal/ORIGINS.md`](legal/ORIGINS.md).

Third-party open-source components keep their own licences, recorded in full in
[`legal/THIRD_PARTY_NOTICES.md`](legal/THIRD_PARTY_NOTICES.md).

---

## Licence

**GNU Affero General Public License v3.0 only.** See [LICENSE](LICENSE).

Use it, study it, change it, pass it on. If you distribute a modified version —
**or run one as a service other people connect to** — you must offer those
people the source, under this same licence. That network clause is the point:
Arcen is remote-access software, and the AGPL is what keeps a modified, hosted
Arcen free as well.

There is no commercial edition, no licence key, and no enforcement code. There
never will be.

---

## If it is useful to you

Development continues while it stays interesting and the tooling bill gets paid.

**[Buy me a coffee](https://wise.com/pay/me/andreasmartina)**

Optional. It buys no support, no priority, and no influence.

And if Arcen helped make something my kids might actually watch — a name in the
tech credits of a children's film would mean more than the coffee. **Andreas
Aanerud.** Entirely optional too, and just as free of obligation, but it would
be a nice thing to point at over breakfast.
