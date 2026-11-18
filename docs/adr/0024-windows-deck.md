# ADR 0024: The Windows Deck

**Status:** Accepted 2026-11-16. Proposed 2026-11-14; revised 2026-11-14
(first-beta scope, decoder reasoning, Native tablet without an Arcen kernel
driver, gamepads); shipped in 0.20.0 (status note below).
Starts step 3 of [ADR 0004](0004-platform-scope.md) for Windows. Builds on
[ADR 0001](0001-workspace-boundaries.md),
[ADR 0014](0014-quic-lanes-and-capture-contracts.md),
[ADR 0016](0016-lossless-refinement.md) and
[ADR 0017](0017-forward-only-pen-input.md).

> Scope: a new product, `clients/windows`, and a twelfth shared crate,
> `shared/deck-core`, extracted from the macOS Deck. No wire change is
> needed for the first release; the shared changes are listed below.

**0.20.0 status note (2026-11-16):** the Windows Deck ships in 0.20.0 as
decided here: `clients/windows` over `shared/deck-core`, its own D3D11
decoder and DirectComposition presenter, the four contracts with the
decoder probe, Tablet support through `WM_POINTER`, and Native tablet
through the optional USB component (WinUSB, a per-install catalog, no Arcen
kernel driver), packaged as two unsigned MSIs
([packaging](../../packaging/windows-deck/README.md)). Presentation follows
the shared per-pipeline timing. Lost connections resume through the shared
reconnect flow, as on the Mac. The Deck has the Mac Deck's display
settings (Match my layout with one fullscreen window per display, up to
four; Primary display only; Windowed), decided by shared code, and greys
out Grading and HDR with the reason when its decoder probe fails, so it
never asks for them. The MSIs ship unsigned: Arcen has no code-signing
certificate, and SmartScreen is documented with the packages. Not yet: a
Windows Pier as a qualified target; the scope in items 6 and 7 below is
otherwise unchanged.

## Context

**The macOS Deck is mostly portable already.** Of its roughly 73,000
production lines, about 15,000 touch macOS at all. A type-check of the whole
Deck crate for `x86_64-pc-windows-msvc` passes for every third-party
dependency (eframe, egui-wgpu, wgpu on DX12, winit, quinn, tokio, rustls,
cpal) and fails in only eight files, all of them Metal, AppKit or
CoreVideo glue; a trial of the first extraction step gated those and the
whole library then type-checked, with the macOS tests unchanged. The obstacle to a second Deck is where the portable code
lives, not the operating system: the controller and every screen sit in one
`ArcenApp` type (`clients/macos/src/ui/app.rs`), and the session engine
(`clients/macos/src/transport/websocket.rs`) imports macOS presenter types.

**Windows forbids less than it seems, but it forbids a few things.**

- The Media Foundation HEVC decoder handles Main and Main10 4:2:0 only.
  Grading and HDR are HEVC 4:4:4 ten-bit
  (`shared/media/src/video/pipeline/mod.rs`, `grading_reference`,
  `hdr10`), so they need the D3D11 video decoder API's HEVC range-extension
  profiles (4:4:4 ten-bit to `Y410`), which drivers expose unevenly: a
  probe found the profile on a Turing-generation NVIDIA GPU and not on a
  Volta-generation one, and neither exposed AV1.
- `egui-wgpu` always chooses an eight-bit swapchain, and wgpu's DX12
  backend never sets a DXGI colour space, so the egui surface cannot carry
  Grading or HDR — the same reason the macOS Deck has dedicated Metal
  layers beneath egui.
- FFmpeg, which most Windows streaming clients use, does not help with
  those profiles: upstream FFmpeg's D3D11 and DXVA hardware paths decode
  HEVC Main and Main10 only, and enabling them compiles in FFmpeg's complete
  software HEVC and H.264 decoders. Its licence is not the obstacle: an
  LGPL-only, dynamically linked build can be combined with an AGPL-3.0
  application, and Arcen's policy (`deny.toml`) admits such a component as
  a recorded exception, which is a blocker for store builds only.
- Native tablet needs real USB transfers from the client, not HID reports:
  the bridge carries control and interrupt transfers
  (`arcen_usb_bridge::CapturedUsbDevice`), and the Linux and macOS hosts
  enumerate the device through it. Windows already ships a signed user-mode
  USB driver, WinUSB; binding it to one device needs only a driver package
  whose catalog the machine trusts, not a new kernel driver. Presenting a
  virtual USB device on a Windows *host* does need a kernel driver, the
  pipeline [ADR 0017](0017-forward-only-pen-input.md) declines to fund.

**Industry practice** for Windows remote-desktop clients is a thin Windows
shell (window, GPU decode and present, input capture, audio) over a portable
session core; decode capability probed from the GPU before it is offered;
a waitable flip-model swapchain; and pen input through the pointer API.
Clients that let their Windows and macOS front ends share nothing have
drifted apart in features.

## Decision

1. **Extract a shared Deck core.** `shared/deck-core` (package
   `arcen-deck-core`) takes the portable Deck logic: settings, saved
   connections and pins, screen state, reconnect state, stream statistics,
   display topology, region, pen and text input. Its default graph is pure
   std and serde. Non-default features add the session engine and lane
   workers (`runtime`: tokio and `arcen-transport/quic`), the portable egui
   screens as view-state functions (`egui`: egui only, no eframe or wgpu),
   and the portable wgpu presenter (`wgpu-presenter`). A tree proof in CI
   keeps tokio, quinn, egui and wgpu out of the default graph. Code that
   belongs to an existing crate goes there: bitstream parsing and decode
   capabilities to `arcen-media`, reconnect policy to `arcen-session`.
   The platform answers the core's questions through one trait,
   `DeckServices` (clipboard, microphone, decode capabilities, display
   colour, typed-pen capability, build identity, config and log
   directories), and decoders implement `VideoDecoderBackend` with an
   associated native-frame type. The existing platform profile,
   `arcen_media::deck_platform::DeckPlatform`, gains a Windows variant.
2. **Extract in order, protecting the macOS Deck at each step:** honest
   `cfg` gates and a Windows type-check gate first; then leaves; then
   settings with golden on-disk fixtures; then bitstream code; then the
   session engine; then the media worker; and last the `ArcenApp` screens,
   one screen per change. Every step keeps the macOS Deck tests and its
   named input and sign-in invariants unchanged.
3. **`clients/windows` is a thin shell.** Package `arcen-deck-windows`, x64.
   It owns only:
   - **Decode.** Media Foundation H.264 into a D3D11 texture for the first
     release; then the D3D11 video decoder API directly, first for HEVC
     4:4:4 ten-bit (brought up on HEVC 4:2:0 first). The bitstream
     parsing, picture buffer and reference-list logic is portable and lives
     in `arcen-media`; the shell only fills the DXVA structures. Parsing is
     scoped to what Arcen Piers emit and anything else is refused with a
     reason. Decoder and presenter share one D3D11 device, on the adapter
     that drives the window's monitor. (Amended 2026-11-18: the decoder
     now runs on a device of its own on that adapter, and each window
     presents on its own device, with pictures shared through NT-handle
     textures and fences, wherever the shared rule
     `decode_device_split` qualifies the driver; otherwise one device as
     before. See `clients/windows/AGENTS.md` and
     [Deck presentation timing](../architecture/deck-presentation-timing.md).)
   - **Present.** A Deck-owned DirectComposition tree: a D3D11 flip-model
     composition swapchain for video at the bottom, and egui drawn by wgpu
     into a caller-owned composition visual on top. The session window
     uses egui-winit and egui-wgpu directly. Video formats and colour
     spaces follow the contract: eight-bit sRGB for Auto and Speed,
     `R10G10B10A2` BT.709 for Grading, `R10G10B10A2` with the PQ/BT.2020
     colour space and HDR10 metadata for HDR — set only when the host
     returns PQ. Ten bits are measured at the output, not assumed: if the
     desktop compositor reduces the picture to eight bits, the Deck reports
     it as a degradation with a visible warning. A waitable object paces
     each monitor's presenter thread; tearing is allowed in Speed only.
     When each frame is presented follows the shared per-pipeline rule
     ([deck presentation timing](../architecture/deck-presentation-timing.md),
     2026-11-15): Auto at once without tearing, Speed at
     once with tearing allowed, Grading and HDR on the refresh.
   - **Input.** Physical scancodes through a shared PC key-position table
     and a PC-keyboard flavour of the shared shortcut policy (Ctrl, Win,
     Alt and AltGr pass through as themselves), committed text as
     `text_commit`, pen through `WM_POINTER` into the existing typed pen
     record, cursor images and positions through the shared pointer
     authority. Pen, pointer and key messages are taken from the window
     procedure before the UI toolkit sees them and never wait for an egui
     pass.
   - **Audio, clipboard, displays.** WASAPI output and microphone (cpal),
     the Win32 clipboard behind the shared clipboard state, Per-Monitor-V2
     DPI and per-monitor colour facts.
   - **Native tablet, as an optional component.** A separately installed
     "Arcen USB component" — a minimal service, separate from the Deck as
     the macOS USB helper is ([ADR 0011](0011-macos-privileged-usb-helper.md))
     — binds the in-box WinUSB driver to the chosen tablet for the length
     of a session and restores the tablet's own driver afterwards. Its
     driver package is signed with a certificate generated on that machine
     for that purpose (code signing only, private key destroyed after
     signing, removed on uninstall). The service re-checks the device with
     the shared tablet admission and serves the same USB frames as the
     macOS helper. Until the component is installed and working, the Deck
     shows Native tablet greyed out with the reason and an install action,
     and advertises it as unavailable; it never falls back silently.
4. **The four contracts on Windows:**

   | Contract | Decode | Present | Offered when |
   | --- | --- | --- | --- |
   | Auto | H.264, then HEVC, 4:2:0 8-bit (AV1 later) | eight-bit, sRGB, each frame at once, no tearing | the H.264 probe passes |
   | Speed | as Auto, low-latency output | as Auto, tearing allowed | as Auto |
   | Grading | HEVC 4:4:4 10-bit to `Y410` | `R10G10B10A2`, BT.709, no HDR metadata, depth measured | the range-extension decoder probe passes |
   | HDR | as Grading | `R10G10B10A2`, PQ/BT.2020, HDR10 metadata | the probe passes and a display reports HDR |

   The probe checks the decoder profile and output format, creates a
   decoder at the advertised size and decodes one embedded keyframe before
   the hello claims the capability. Where it fails, the Deck says why and
   does not fall back silently. Exact still image runs on Grading and HDR
   through a D3D11 implementation of `arcen_media::refine::hub::RefineCompositor`,
   with uploads ordered on the presenter's own device context before the
   conversion pass, as ADR 0016 requires.
5. **Secrets and files mirror the macOS Deck.** No password is stored;
   trust-on-first-use pins and settings live under `%APPDATA%\Arcen\Deck`,
   logs under `%LOCALAPPDATA%\Arcen\Deck\Logs`.
6. **Out of scope for the first release:** general USB redirection, any
   Arcen kernel driver, ARM64 releases, AV1, touch, Windows Server
   multi-session clients, remembered passwords. Windows 11 is the supported
   client OS. Gamepads follow in a later release through the typed devices
   of [ADR 0018](0018-unified-typed-devices.md), captured on both Decks
   through SDL3's gamepad layer (zlib).
7. **The first beta** connects to Linux hosts (X11 and Wayland) and the
   macOS host, with Auto and Speed, Grading and HDR where the decoder probe
   passes, keyboard, pointer, clipboard and audio, and pen tablets in both
   modes: Tablet support (pointer-API pen, no driver) and Native tablet
   (the optional component above). Windows hosts as targets, a tablet on a
   Windows host and store distribution follow the beta.

## Consequences

- A twelfth shared crate, with its consumer register, validation profile,
  purity proof and the root crate table. The macOS Deck shrinks to its
  AppKit, Metal and VideoToolbox shell.
- `cargo xwin check --target x86_64-pc-windows-msvc -p arcen-deck-macos --lib`
  (later `-p arcen-deck-core --all-features`) becomes a pre-merge gate, so
  a portable change cannot silently become macOS-only again. It runs on
  developer machines first; running it in CI, which downloads the Windows
  SDK, is a Release/Security decision.
- Shared additions: the PC key-position table and keyboard policy in
  `arcen-input`; a
  Windows constructor for `arcen_media::display_color` (headroom from the
  display's maximum luminance and the SDR white level); the bitstream and
  decode-capability model, H.264/HEVC parsing and picture-buffer logic in
  `arcen-media`; `DeckServices` in `arcen-deck-core`; the Native tablet
  component's state and its user-facing reasons, shared by both Decks. None
  changes the wire.
- Grading and HDR on Windows depend on GPU drivers that expose HEVC 4:4:4
  ten-bit through D3D11. Where they do not, those presets are refused with
  a reason until another route (Vulkan Video, or a vendor decode API) is
  built; that is tracked work, not a statement that it is impossible.
- No codec is redistributed: Media Foundation and the GPU driver implement
  decoding. Codec licensing for Arcen's own parsing code is reviewed before
  the D3D11 decoder ships.
- The Native tablet component adds a machine-trusted, per-install
  certificate. It is optional, constrained to code signing, its key is
  destroyed after use, and its removal on uninstall is a release check.
  Whether the driver package installs with Secure Boot on is proven on real
  hardware before the beta.
- A route for Native tablet on Windows hosts exists without an Arcen kernel
  driver: an operator-installed, unchanged copy of usbip-win2 (BSD-2-Clause),
  whose releases are Microsoft-signed, attached to a USB/IP server in the
  host built on the shared `arcen_usb_bridge` USB/IP codec. That is a
  proposed amendment to ADR 0017, not part of this decision. (2026-11-16:
  the Windows Pier takes Native tablet by another driverless route instead,
  reading the pen itself; ADR 0023 amendment.)
- The Windows Deck needs Authenticode signing for its executable and
  installer before release; that is a Release/Security decision, as are the
  installer format and the third-party notices for the new dependencies.
  (2026-11-16: decided for 0.20.0 — the MSIs ship unsigned, with the
  SmartScreen steps documented; the notices ship in both MSIs.)
- A Linux Deck becomes a decoder, a presenter and adapters over the same
  core.

## Alternatives considered

- **A Windows Deck written from scratch** in a Windows UI framework: fastest
  first picture, then permanent drift from the macOS Deck's session,
  reconnect, input and refinement behaviour.
- **FFmpeg's D3D11 hardware decoding:** admissible as an LGPL-only,
  dynamically linked build in direct downloads, but upstream FFmpeg does
  not decode HEVC 4:4:4 through D3D11, its error concealment conflicts with
  keyframe recovery and exact still images, and it ships a complete
  software decoder. It remains a recorded fallback for the eight-bit HEVC
  path if Arcen's own decoder slips.
- **Raw HID reports for Native tablet:** cannot answer the standard USB
  requests the hosts send, and Windows opens pen collections exclusively.
- **UsbDk, libusbK or an own filter driver** for capture: each is a kernel
  driver with its own signing question; WinUSB is already in the box.
- **Media Foundation only:** cannot decode HEVC 4:4:4, so no Grading or HDR.
- **Video through egui's own surface:** eight-bit only and no colour-space
  control; acceptable for an early Auto build, not for the fidelity
  contracts.
- **A child window for video under a transparent UI window:** wgpu already
  takes the window's lower composition target for egui, so a child window
  would sit above the overlay unless the overlay moved to the topmost
  target; and a child window costs input routing and independent-flip
  eligibility. One Deck-owned composition tree avoids all three.
