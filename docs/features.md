# Arcen features and status

Moved from the README (0.19.0). The README keeps a short quick start; this
page keeps the detail.

**The span between you and your machine.** Arcen streams a real desktop from a
powerful workstation to a thin client, over an encrypted direct connection.
Native Rust throughout. Free software under the **AGPL-3.0**. Anyone can
download, build and run it. Official signed builds, cloud workstation images
and support come from TUTTIFRUTTI AS — see
[Licensing and official builds](project.md#licensing-and-official-builds).

## What Arcen aims to be

The best remote desktop for media and colour-critical work: a Mac in front of
you, a workstation somewhere else — Linux on X11 or Wayland, Windows, or a
Mac — and nothing lost in between. Three things decide that, and each is
measured rather than claimed:

- **Exact colour.** Grading and HDR are their own pipelines from capture to
  the Deck's glass, never labels on the eight-bit fast path. The host says
  what it actually serves, and the Deck's [Colour check](#streaming-presets-and-independent-pipelines)
  compares every decoded pixel with a known pattern.
- **A real pen.** Pressure, tilt, eraser and buttons reach the host
  application as a pen, from any HID tablet, whoever made it.
- **Native feel.** The Mac's own cursor showing the host application's
  cursor, the Mac keyboard mapped the way the host expects, and the GPU fast
  path at 60 fps when you ask for it.

[What works today](#what-works-today) says where each host stands;
[Roadmap](project.md#roadmap) says what comes next.

---

## Start here

| You are | Start with | When something is wrong |
|---|---|---|
| **An artist on a Mac** | The [user guide](user-guide.md), in the order you need it: install, connect, presets, colour check, pen, keyboard, displays, reconnecting and signing out. In short: [install the Deck](install.md#the-client-deck), then [Connect](#connect): an address, a fingerprint to confirm once, your usual account on the host. Pick a [streaming preset](#streaming-presets-and-independent-pipelines): Auto, Speed, Grading or HDR. | Open the connection and choose **Test connection**. It checks the whole way to the host without your password and says what to do; **Copy report** gives your administrator the details. Then [Troubleshooting](operations/troubleshooting.md). |
| **An administrator installing a Pier** | [Install the Pier](install.md#the-host-pier), then the [administration guide](operations/pier-administration.md). Every `pier.json` key, with its default and the release that introduced it, is in the [configuration reference](operations/config-reference.md). | Every code in the Pier's log or on a user's screen is in [Error codes](operations/error-codes.md); alerting recipes are in [Monitoring](operations/monitoring.md); `arcen-deck diagnose <host>` tests a Pier from any Mac. |
| **An IT security reviewer** | [Security](how-it-works.md#security), [trust boundaries](security/trust-boundaries.md), [SECURITY.md](../SECURITY.md), the [supply chain](security/supply-chain.md), and the Linux Pier under [SELinux](security/linux-selinux.md) and polkit (in the [administration guide](operations/pier-administration.md#polkit)). | Before sign-in a Pier says only how to sign in (methods, an optional notice, whether it offers several displays and session resume) — no version or build. One UDP port, TLS 1.3, operating-system authentication, no Arcen user database. |

---

## What works today

Read this before you invest time.

| | State |
|---|---|
| **Linux Pier** (host) | Works, on **X11** and — new in 0.17.0 — **Wayland** (GNOME Shell/Mutter). The Pier chooses from what the host has, never from the distribution's name: an installed Xorg server keeps the dedicated X11 desktop, and an existing installation never moves; without Xorg, each sign-in gets its own GNOME desktop, which the installer qualifies on the host before the service starts and the service re-qualifies after updates. **X11:** Auto and Speed on the NvFBC GPU path (Speed at 60 fps on a head paced at 120 Hz), Grading on a separate depth-30 XShm path for genuine 10-bit SDR, HDR visibly served as Grading unless the operator declares a Rec.2100 PQ desktop; one X11 desktop per host. **Wayland:** verified end to end on Rocky Linux 10.2 / GNOME 49 with an NVIDIA GRID vGPU (GNOME 40 on Rocky Linux 9 is a tested baseline); other GNOME hosts are admitted only by the same qualification. Auto everywhere, Speed where qualification proves it, Grading and HDR on GNOME/Mutter 50 or newer, and a real pen with pressure and tilt on stock GNOME when the desktop runs natively on its own seat (one per host). Everything Wayland cannot do yet is served visibly lower, with the reason — see [X11 and Wayland compared](#linux-pier-x11-and-wayland-compared) and [How Wayland support works](platforms/linux-wayland.md#how-wayland-support-works). On both, the Pier keeps the host from sleeping while it serves a desktop, and a remote desktop gets polkit's remote answers, never the console's, so its user cannot power off, reboot or suspend the host, change its networking or mount disks without an administrator (fixed in 0.17.0; polkit older than 0.106 cannot be given Arcen's rule, and the Pier logs a `SECURITY:` error there — see [CHANGELOG](../CHANGELOG.md)). |
| **Windows Pier** (host) | Works. Multi-display is automatic: the service picks an eligible NVIDIA adapter at start, and an administrator can reserve a GPU for other work. Auto/Speed use the 8-bit capture path; Grading uses WGC FP16 scRGB re-encoded with the sRGB curve Windows linearised with, so a 0.18.0 Deck gets the desktop's own code values (older Decks get the BT.709 curve); HDR provisions and verifies an HDR head before a separate FP16-to-PQ/BT.2020 encode path. |
| **macOS Deck** (client) | Works. Signed and notarised. On Apple silicon VideoToolbox hardware-decodes H.264 and HEVC, including 4:4:4 10-bit. A dedicated 10-bit Metal layer presents Grading in SDR and enables PQ/EDR only for a host-confirmed HDR stream. Two apps from one source, both for macOS 12 or later: **Arcen Deck** for Apple silicon and **Arcen Deck Intel** for Intel Macs. AV1 decode needs Apple silicon M3 or later (macOS 14); the Intel app never offers AV1, and on Intel VideoToolbox may decode in software. Native tablet needs macOS 13, and on an Intel Deck it is untested on hardware. |
| **Linux Deck, Windows Deck** | Do not exist. [Help wanted.](project.md#where-help-is-wanted) |
| **macOS Pier** | Preview. A signed, notarised package ships with each release, and a logged-in user can serve a real Deck: PAM sign-in, ScreenCaptureKit → VideoToolbox video in Auto, Speed and Grading, HDR when the display being captured has HDR headroom, one virtual display per Deck display, pointer and keyboard input, typed text (input method, Characters mode and Paste as Keystrokes, since 0.19.0), a pen that applications see as a tablet (pressure, tilt, rotation, eraser, barrel buttons — pressure reaches them since 0.19.0) and trackpad gestures, clipboard both ways, host audio, and session-scoped time-zone redirection for newly launched apps. It installs as a network service under a hidden `_arcen` account plus a helper in each signed-in session ([why two parts](security/macos-pier-process-model.md)). A virtual microphone (an AudioServerPlugIn) ships in the package and negotiates with the Deck, but end-to-end microphone capture has not been qualified. Native tablet on a macOS Pier (since 0.19.0): pressure, tilt, eraser and barrel buttons for a tablet model the Pier knows (the Wacom Intuos5 touch L, read by the Pier itself; proven with a real Intuos5); any other model is refused with a reason. Not yet: deskside privacy and qualification of cold-boot login-window operation and of multi-display on physical hardware. FileVault's pre-boot unlock screen cannot be served. |

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
at start (`capture.provider`, see [How Wayland support works](platforms/linux-wayland.md#how-wayland-support-works)).

| | X11 (an Xorg server is installed) | Wayland (GNOME Shell/Mutter, no Xorg) |
|---|---|---|
| **Desktop** | A dedicated Xorg server on the console seat; one per host | Headless: a seatless GNOME session per sign-in. Native: the same stock compositor on its own logind seat, one per host |
| **Auto / Speed** | NvFBC → CUDA → NVENC, frame stays on the GPU; Speed at 60 fps on a head paced at 120 Hz | Mutter ScreenCast → PipeWire DMA-BUF → EGL → CUDA → NVENC with explicit sync, where qualification verified the import (otherwise a host copy). Speed only where qualification measured the rate at that display size, on one output; otherwise visibly Auto |
| **Grading** | Depth-30 Xorg → XShm RGB10 → shared conversion → NVENC 4:4:4 10-bit, BT.709. The desktop must be created at depth 30 (`video.x11_desktop_depth`) | GNOME/Mutter 50 or newer after a per-host precision proof, one output, HEVC 4:4:4 10-bit BT.709. GNOME 49 (Rocky Linux 10) only through the optional [side-by-side GNOME prefix](operations/pier-administration.md#upgrading-the-gnome-session-on-el10); otherwise visibly Auto |
| **HDR** | Served as Grading, unless the operator declares a Rec.2100 PQ desktop (`video.desktop_encoding`) | Real BT.2020/PQ from the compositor on GNOME/Mutter 50 or newer, same proof and limits as Grading |
| **No NVENC** | OpenH264, H.264 4:2:0 | OpenH264, H.264 4:2:0, up to 30 fps, outputs fitted to 1920×1200 |
| **Monitors** | One to four; NVIDIA heads discovered at start | One to four Mutter virtual outputs, Remote UI scale, Windowed resize; Speed, Grading and HDR on one output only |
| **Cursor** | The Mac's own cursor drawing the host's cursor images (XFixes) | The Mac's own cursor drawing the host's cursor images, where qualification proves Mutter's cursor metadata; never a second cursor in the video |
| **Pen (Tablet support)** | Virtual tablet: pressure, tilt, eraser, barrel buttons | Native desktop: a real pen with pressure and tilt in Wayland and Xwayland applications. Headless desktop: acts as a mouse until GNOME's EIS offers a stylus |
| **Native tablet (USB bridged)** | Administrator opt-in (`usb.native_tablet = "enabled"`, or `install-arcen-pier --native-tablet`), on a LAN, through the distribution's in-tree USB/IP `vhci-hcd` (Rocky/RHEL: `kernel-modules-internal`, installed by the installer only when Native tablet is enabled). The tablet reaches only your desktop, never the login screen, and joins an X11 desktop on the first connection ([ADR 0021](adr/0021-bridged-usb-devices-on-their-own-seat.md)). Proven with a real Wacom Intuos5: first-connection hot-plug, drawing with pressure, the Deck's pen dot and hand-back. The dot follows the pen's own evdev reports (a tablet remapped in X falls back to the X pointer); application ink follows xf86-input-wacom's smoothed X pointer | Native-seat desktops only (same opt-in and controller): the tablet joins the desktop's own seat and the Deck's pen dot follows the pen's own evdev reports; proven with a real Wacom Intuos5 drawing with pressure. GNOME squares stylus pressure (raise the curve in Settings); a tablet remapped in GNOME Settings offsets the Deck's cursor; no cursor positions with several outputs. Headless desktops refuse it |
| **Keyboard** | Keys, plus typed text (input method, Characters mode, Paste as Keystrokes): by key where the session's layout has the character, any other character through a briefly borrowed unused key | Keys, plus typed text the session's keymap proves it can type, by key with Shift and AltGr; other characters (and, with several differing layouts, those they disagree on) are refused and counted until GNOME offers `ei_text` |
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
| **Microphone in** | **Linux hosts; macOS Pier preview.** Client microphone into the host session, Opus or fixed-rate PCM. Enabled in the host's default configuration, and still opt-in on the Deck every launch — consent is deliberately never restored from settings. Windows needs a signed driver Arcen does not yet ship (see [Where help is wanted](project.md#where-help-is-wanted)). The macOS Pier publishes a virtual microphone and offers it only when that device is present; end-to-end capture is built but not yet qualified. |
| **Keyboard and pointer** | Absolute and relative motion, and negotiated cursor authority — the host draws the cursor, or the client does, but never both. A client-drawn cursor shows the host application's own cursor image (X11, Windows, Wayland with cursor metadata; named shapes on the macOS Pier), sent once and drawn at its size in the video. Trackpad scrolling stays precise end to end (phased deltas on macOS, high-resolution wheel on Linux, fractional and horizontal wheel on Windows). Trackpad gestures — magnify, smart zoom, swipe — are negotiated separately and injected where the host OS allows it (the macOS Pier today). |
| **Typed text** | **Windows, Linux Xorg, Linux Wayland and macOS hosts.** What the Mac's input method composes (accents and dead keys, Japanese, Chinese, Korean, emoji) and keys the remote layout would type differently arrive as the characters you typed; everything else, shortcuts and games included, still goes as keys (Settings → Input → Typing, and per connection or per session **Keyboard: Characters**). **Paste as Keystrokes** types the Mac's clipboard where clipboard sharing cannot reach — a sign-in or lock screen, a console. The host decides per character: by its key on the host's own layout when that layout provably types it, else by its text route (Windows Unicode input; on Xorg a briefly borrowed unused key; on Wayland libei's `ei_text` when the compositor offers it). On GNOME Wayland today a character is typed only by keys the session's keymap proves; characters outside the layout need `ei_text` (not in a GNOME release yet) and are refused visibly. The macOS Pier (since 0.19.0) types every character as a Unicode key event, whatever the Mac's layout — emoji, flags and combining accents included — wherever it may post keys (the Accessibility grant keys need; not at the login window, where it advertises no text); its Option keys stay Option by default (the **Automatic** Option-keys setting). |
| **Pen and tablet** | Any HID pen tablet, whoever made it: a device counts as a tablet because its HID report descriptor declares a pen, never because of its vendor id. Three modes, chosen per connection, because the choice is really about the network — it decides where the pen is interpreted.<br><br>**Tablet support** *(default, any distance)* — the Mac's own tablet driver reads the pen and Arcen sends finished pen events. Nothing waits for a reply and the host needs no tablet driver. Pressure, tilt, rotation, eraser, proximity and barrel buttons all work; finger touch and the tablet's own buttons stay on the Mac, which keeps working in Mac applications.<br><br>**Native tablet (USB bridged)** *(LAN only; Linux Piers whose administrator enabled it, on X11 and native-seat Wayland desktops, and macOS Piers, with pressure for known models; see below)* — a privileged helper takes the device from macOS and forwards raw USB, so the host's own driver for that tablet claims it. The whole device works, including finger touch and the tablet's buttons. Every sample makes a full round trip, and Mac applications lose the tablet until you disconnect.<br><br>**Mouse compatibility only** — no redirection; the pen acts as a mouse.<br><br>On Linux Wayland hosts Tablet support gives a real pen (pressure and tilt) on a desktop that runs natively on its own seat (`wayland.session_mode`, one per host); on a headless desktop the pen acts as a mouse until GNOME's EIS offers a stylus. Native tablet joins a native-seat Wayland desktop and is refused on a headless one. See [How Wayland support works](platforms/linux-wayland.md#how-wayland-support-works). On a macOS Pier the pen arrives as the tablet events a Mac tablet driver posts — proximity with pen or eraser, pressure, tilt, rotation and barrel buttons — with no entitlement or driver on the host; a virtual HID digitizer was measured and rejected because macOS hands its pressure and tilt to no application ([details](../hosts/macos/AGENTS.md#a-virtual-pen-digitizer-measured)). At the macOS login window the pen is a plain pointer. On Windows the pen is a Windows Ink pen: set drawing applications to Windows Ink ([per-application settings](operations/pen-tablet-input.md#4a-windows-applications-choose-windows-ink)); Arcen does not support Wintab, so an application that speaks only Wintab gets no pressure ([ADR 0017](adr/0017-forward-only-pen-input.md)). Where Native tablet is not offered (Windows, a Linux host that has not enabled it, a headless Wayland desktop) the mode is refused rather than quietly downgraded, so a Deck that asks for it is told why and keeps ordinary mouse control. On Windows that is a decision, not a gap: it would need a signed kernel driver the project does not fund (ADR 0017). A macOS Pier publishes the bridged tablet with `IOHIDUserDevice` under the `com.apple.developer.hid.virtual.device` entitlement ([ADR 0022](adr/0022-macos-pier-native-tablet-hid-adapter.md)). macOS marks such devices virtual and the tablet maker's driver does not bind them, so for a model whose pen packets Arcen knows (the Intuos5 touch L) the Pier reads the pen itself and posts tablet events with pressure, the route the maker's own driver uses ([ADR 0022 §14.1](adr/0022-macos-pier-native-tablet-hid-adapter.md)); any other model is refused with a reason (§14.2). On every host the Deck draws a white pen dot where the bridged pen points ([one path](adr/0023-one-native-tablet-pen-position-path.md)). |
| **Clipboard** | Text and images, both directions, or restricted to one, or off. The host decides; the client cannot override it. |
| **Timezone** | The session follows the client's timezone, so timestamps read the way you expect. Linux sets `TZ` in the remote process tree; Windows temporarily changes the system timezone; the macOS Pier sets `TZ` in the served GUI session, so apps launched during the session see the Deck zone while the menu bar clock and already-running apps stay on the Mac's zone. |
| **Login banner** | Off by default. A host can require the user to read and accept an operator-written notice *before* the Deck collects any credentials, with the exact text recorded. Useful where a legal warning is mandatory. |
| **Reconnection** | A dropped connection resumes the same session within the host's reconnect window: three minutes by default, up to two hours if the administrator sets `auth.reconnect_window_secs`. The Deck reattaches with a signed grant instead of asking for the password again. |
| **Bitrate** | Follows the network path through one shared controller on every host, and QUIC uses BBR, so the random loss of Wi-Fi, cellular and VPN links is not mistaken for congestion (a loss-bounded variant, `bbr-guarded`, is an opt-in experiment for slow bottlenecks). The Deck's Connection Health view shows the path as the Pier measures it: RTT, loss, rate, controller and packet size (MTU) (every Pier; the macOS Pier since 0.19.0); Session → Stream Statistics shows the same path in a compact box with frame rate, codec, SDR/HDR and lossless coverage. When an encoder cannot sustain a preset's frame rate the session is admitted at the best proven rate and says so, instead of being refused. |
| **Deskside privacy** | Linux and Windows hosts. Off by default. When enabled, the physical screen is blanked and its keyboard and mouse are disabled for the duration of a remote session, so nobody standing at the machine can watch or interfere. Input and display are locked together — you cannot get one without the other. Its physical acceptance runs are still open, so treat it as unqualified ([deskside recovery](operations/deskside-recovery.md)). |

### Streaming presets and independent pipelines

Arcen does not take one 8-bit capture path and relabel it for every mode. The
presets choose complete contracts, and each host selects a capture provider
that can actually supply that contract.

| Preset | Requested stream | Windows Pier | Linux Pier (Xorg) | macOS Pier (preview) | Deck presentation |
|---|---|---|---|---|---|
| **Auto** | 30 fps, adaptive 4:2:0 8-bit | DDA when it proves real frames, otherwise WGC BGRA8 | NvFBC → CUDA → NVENC; device-to-device fast path | ScreenCaptureKit → VideoToolbox; hand-qualified as `420v`/`Yuv420` on one lab Mac | Ordinary SDR VideoToolbox path |
| **Speed** | 60 fps, adaptive 4:2:0 8-bit | Same 8-bit capture path at the higher cadence | Same NvFBC fast path at the higher cadence | Same macOS 8-bit path at the higher cadence; not a release qualification | Ordinary SDR VideoToolbox path |
| **Grading** | 30 fps, HEVC 4:4:4 10-bit full-range BT.709 | WGC FP16 scRGB → sRGB re-encode (desktop codes, tagged `srgb`; BT.709 curve for older Decks) → matrix → NVENC P16 | Depth-30 Xorg → XShm RGB10 (desktop codes, tagged `srgb` for 0.18.0+ Decks) → shared conversion → CUDA upload → NVENC P16 | ScreenCaptureKit 10-bit 4:4:4 in sRGB (desktop codes, tagged `srgb` for 0.18.0+ Decks) → VideoToolbox; hand-qualified as `x444`/`Yuv444` on one lab Mac | Native `xf44` → dedicated `RGB10A2Unorm` Metal layer, SDR |
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
[`docs/architecture/color-fidelity.md`](architecture/color-fidelity.md).

**Colour check.** The Deck's **Session › Colour Check…** (also Settings ›
Streaming) is a colourist's checklist with a ✓/⚠/✗ verdict per item: the
preset, codec profile, chroma, depth, transfer, primaries, matrix and range the
host actually serves against what was asked for, with the host's reason for
any downgrade and the headers of the frames decoded; this Mac's reference
mode, colour profile, HDR headroom, ten-bit Metal layer, EDR state and 1:1
scaling; and an on-demand measurement of a known test pattern shown on the
host, compared on every pixel in the stream's own code values (for example
`max 0/1023`), not as a share of "flat" pixels. One click switches to Grading
or HDR, enters fullscreen or turns on Retina-resolution streaming; host-side
prerequisites (Linux depth 30, Mutter 50, Windows HDR) are explained and
linked, never changed. **Copy report** and **Save report** produce a
timestamped text report to attach to a review. A lossy stream is measured as
close, not claimed bit-exact; bit-exact still frames are the job of lossless
refinement ([ADR 0016](adr/0016-lossless-refinement.md), proposed). See
the [colour check guide](operations/colour-check.md).

Native tablet needs a host that can present the tablet on a virtual USB
controller so the host's own driver can claim it. **Linux hosts can, when the
administrator enables it** (`usb.native_tablet = "enabled"` in `pier.json`, or
`install-arcen-pier --native-tablet`; every Linux Pier build carries it). It
uses the distribution's own, signed USB/IP `vhci-hcd`; on Rocky Linux and RHEL
it ships in `kernel-modules-internal`, which the installer installs only when
Native tablet is enabled, see [ADR 0020](adr/0020-linux-native-tablet-in-tree-vhci-hcd.md)
and the security model in [ADR 0021](adr/0021-bridged-usb-devices-on-their-own-seat.md).
**macOS hosts offer it, with pressure for known models**: the Deck captures
the tablet with Apple's IOUSBHost, and a macOS Pier signed with
`com.apple.developer.hid.virtual.device` takes it as the USB host
([ADR 0022](adr/0022-macos-pier-native-tablet-hid-adapter.md)). For a
model whose pen packets Arcen knows (the Wacom Intuos5 touch L) the Pier
reads the pen itself and posts tablet events with pressure, tilt, eraser and
barrel buttons, and the Deck's cursor follows the pen (measured on the lab
Pier with an emulated Intuos5: pressure 0.059 to 1.000; a real tablet is not
yet measured). Any other model is refused, with its ids, rather than
half-working; adding models to the reader is the follow-up.

**Native tablet: one path.** Every Pier reports where the bridged pen points
the same way ([ADR 0023](adr/0023-one-native-tablet-pen-position-path.md)):
a thin platform adapter reads the pen's own reports (the evdev node on
Linux, X11 and Wayland alike; the Pier's pen reader on macOS), shared code
decides that the pen leads while it is in proximity and maps it, and the
position goes to the Deck the moment each report arrives. The Deck draws a
white pen dot there on every host. An Xorg desktop whose tablet the user
remapped (tablet area, rotation, output mapping) falls back to reading the X
pointer. What you see and the known traps (a held side switch looks like
"pen but no ink"; Safari reports every pen as a mouse) are in the
[pen and tablet guide](operations/pen-tablet-input.md#native-tablet-one-path).
**Windows hosts do
not offer Native tablet.** It would need a signed kernel driver (a virtual USB
host controller): an EV code-signing certificate, Microsoft attestation
signing, and WHQL/HLK for broad distribution. The project does not fund that,
so the mode is not offered rather than planned; see
[ADR 0017](adr/0017-forward-only-pen-input.md). Tablet support is the
Windows pen and needs nothing installed there.

On macOS the privileged part of Native tablet is a small root daemon registered
through `SMAppService`, not the Deck: Arcen Deck itself never runs as root.
Installing it is one approval in Login Items. See
[ADR 0011](adr/0011-macos-privileged-usb-helper.md).

Keyboard, pointer and scroll are synthesised by the hosts: they arrive as
native events the operating system generates on Arcen's behalf. On Linux this
goes through the kernel's own input layer; on Windows through the injection API;
on the macOS Pier, pointer/scroll injection works without Accessibility and
the keyboard needs the Accessibility approval the helper asks for when it
starts. At the macOS login window, where synthetic events do not reach, the
Pier types through a virtual HID keyboard and pointer.

**Not supported:** webcam redirection of any kind, and USB passthrough for
anything other than the one tablet class above. Neither is a small gap —
[`docs/architecture/macos-peripheral-access.md`](architecture/macos-peripheral-access.md)
sets out what a webcam would take and which of the two routes to try first.
**Wintab and Native tablet on Windows hosts are not supported either**, by
decision: Windows hosts deliver pens through Windows Ink only
([ADR 0017](adr/0017-forward-only-pen-input.md)).

Arcen expects a **trusted network**. It is not hardened against the open
internet. Put it behind a VPN or a network you control.

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

### Your Mac keyboard on the host

The Deck learns what the host is from its hello and maps the Mac's keys the
way that host expects. Each saved connection can change it (Edit connection →
**Keyboard shortcuts**, and **Left Option is also AltGr**).

| Host | ⌘ Command | Control | ⌥ Option |
| --- | --- | --- | --- |
| Mac | ⌘ | Control | Option, both keys (also when the Mac host types text; see below) |
| Linux | Ctrl | Ctrl | Alt |
| Windows | the Windows key; ⌘C, ⌘X, ⌘V, ⌘A, ⌘Z, ⌘F and ⌘W are Ctrl+C, X, V, A, Z, F, W | Ctrl | left: Alt; right: AltGr |

#### Keyboard on Windows hosts

The default follows Microsoft's Windows App for Mac:

- ⌘ on its own opens Start; ⌘ with another key is the Windows key with it
  (⌘E opens File Explorer, ⌘R opens Run). ⌘ with a click, scroll or drag is
  the Windows key with it, and letting go of ⌘ afterwards does not open Start.
- ⌘C, ⌘X, ⌘V, ⌘A, ⌘Z, ⌘F and ⌘W are sent as Ctrl with that key, never as
  Windows+Ctrl; ⌘⇧Z is Ctrl+Shift+Z. ⌘Q tapped goes to Windows as Windows+Q
  (hold it to quit the Deck).
- Right ⌥ is AltGr, so the characters a European Windows layout puts on
  AltGr come out where that layout has them: on a Norwegian layout right ⌥2
  types `@`, right ⌥7 `{`, right ⌥E `€`. Tick **Left Option is also AltGr**
  to type them with left ⌥ too; left ⌥ is otherwise Alt, for shortcuts.
- Keys are sent by position and Windows' own keyboard layout decides the
  character, exactly as for a keyboard plugged into the PC. Choose the
  layout that matches your Mac keyboard on the Windows side (Settings → Time
  & language → Language & region). The ISO key next to left ⇧ (`<>` on
  European keyboards) is its own key.

On a host that types text, what each ⌥ Option key does is the **Option keys**
setting (Settings → Input → Typing). **Automatic**, the default, keeps both
Option keys as Option on a Mac host, as before it typed text, and on other
hosts makes left ⌥ Alt and right ⌥ type this Mac's special characters; a mode
chosen there applies to every host
([`OptionKeyPreference`](../shared/input/src/text/policy.rs)). Input-method
text, the characters Characters mode sends and Paste as Keystrokes arrive as
text either way; an ⌥ key that stays Option is a key, so the host's own
layout decides what ⌥ chords type.

The other choices: **⌘ and Control swapped** sends ⌘ as Ctrl for every key and
Control as the Windows key (as Parsec's swap); **⌘ is Control** is what Arcen
does for Linux. A Mac host always keeps ⌘ as ⌘. Older Piers that do not
announce AltGr and the ISO key get ⌥ as Alt and the ISO key as before.

#### Physical keys or characters

Keys normally go by position, and the host's keyboard layout decides each
character — the right choice for games, shortcuts and anything that reads
keys. When the host's layout differs from the Mac's (a US Windows layout under
a Norwegian Mac keyboard), switch the session to characters:

| Connection menu | Shortcut | What reaches the host |
| --- | --- | --- |
| **Keyboard: Physical Keys** | ⌃⌘K | Keys by position, as the Typing setting says (input-method text still arrives as text) |
| **Keyboard: Characters** | ⌃⌘U | Every printable character as this Mac typed it — `æøå`, dead-key accents, Option characters, input methods — whatever the host's layout is |

The shortcuts are the ones Microsoft's Windows App uses for its scancode and
Unicode modes. The choice lasts for the session and survives reconnects to
the same host; a saved connection can start in either (Edit connection →
**Keyboard mode**). In Characters mode the video shows a small **Keyboard:
Characters** badge. Shortcuts (any ⌘ or Control chord), arrows, function keys,
Esc, Return, Tab and Backspace stay keys. Trade-off: a game, a remote-control
or virtual-machine console, or any application that reads key positions may
see a character rather than a key (the host still types a character by its
key where its own layout has it and no key is held), and a held letter
repeats as typed characters rather than as a held key.
Characters mode needs a host that types text (Windows, Linux Xorg and Wayland
Piers from 0.18.0, the macOS Pier from 0.19.0); a host that cannot is said so
and the keys stay physical.

#### Paste as Keystrokes

**Connection → Paste as Keystrokes** (⌃⌥⌘V) types the Mac clipboard's plain
text into the host as keystrokes — for the places clipboard sharing does not
reach: the Windows sign-in, lock and elevation screens, a Linux lock screen, a
virtual-machine or firmware console.

- **It never types under your fingers.** Every key the shortcut held is
  released on the host first, and typing waits until you let go of every
  modifier, so no character becomes a shortcut.
- **Paced and bounded.** It types one character at a time, 25 a second by
  default — Windows 11's Notepad dropped keys at 50 in the lab, so faster is
  for applications you know keep up, slower (Settings → Input → Paste as
  Keystrokes) for consoles that drop keys. Up to 2,000 characters are typed at once; longer text asks first,
  saying how long it will take; nothing over 100,000 characters is typed.
- **Esc stops it** between two characters, as does **Connection → Stop
  Typing**; keys you press while it types are not sent, so nothing lands in
  the middle of the text. Progress and the outcome are shown over the video.
- **Line breaks are Return and tabs are Tab.** A line break at the very end
  is not typed, so a copied password never submits a form by itself — press
  Return yourself. Other control characters are left out.
- **It types what the clipboard says, never a guess.** The host types each
  character by its key where its own layout provably has it, and through its
  text route otherwise (Windows Unicode input; a briefly borrowed unused key
  on Xorg; `ei_text` on Wayland where GNOME offers it, otherwise the
  character is refused and counted; Unicode key events on a macOS Pier). A
  host without a text route (the macOS login window, Piers before 0.18.0, a
  macOS Pier before 0.19.0) refuses with a message instead of sending key
  positions its layout might turn into other characters.
- **Private.** The text is never logged, by the Deck or the host — only
  counts — and characters a host could not type are counted, never quoted.
  An administrator whose clipboard policy keeps this Mac's clipboard out of
  the host keeps it out of Paste as Keystrokes too.

#### Testing the way to a host

To check the way to a host without signing in at all:

```sh
"/Applications/Arcen Deck.app/Contents/MacOS/arcen-deck" diagnose <host[:port]>        # Apple silicon
"/Applications/Arcen Deck Intel.app/Contents/MacOS/arcen-deck" diagnose <host[:port]>  # Intel
```

It tests name resolution, UDP 18444 with a real QUIC handshake, the
certificate against the identity this Deck remembers, the host's greeting
before sign-in, the path MTU and the round trip, and prints what to do about
anything that fails. It sends no user name or password. The same test is
**Test connection** in the Deck's connection editor. Its codes are explained
in [Error codes](operations/error-codes.md).

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
