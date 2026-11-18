# Windows Client Ownership

**Owner role:** Windows Client

Own the Windows Deck (ADR 0024): its Win32 window, D3D11 video decode and
presentation, input capture, audio, clipboard, display facts, and the
optional Arcen USB component (`usb/`). Windows 11 x64 only; ARM64 is
check-only later.

This crate is a **thin shell**. Everything portable lives in
`shared/deck-core` (`arcen-deck-core`) and the other shared crates; this
crate implements the three seams and nothing else:

| Seam (`arcen-deck-core`) | Implemented in |
| --- | --- |
| `DeckServices` | `src/platform/`, with `src/audio/`, `src/clipboard/`, `src/display/`, `src/logging/` |
| `decode::VideoDecoderBackend` + `VideoDecoderFactory` | `src/decode/` (our own `ID3D11VideoDecoder`/DXVA decoder) |
| `present::VideoPresenterSink` (+ `RefineCompositor`) | `src/present/` (DirectComposition, flip-model swapchains) |

Before writing anything here, ask the root `AGENTS.md` question: could it
compile and pass its tests on another operating system? If yes, it belongs
in `shared/`. Reuse the macOS Deck's logic, wording and screens; the two
Decks must look and behave the same (1:1 parity).

Validate with `cargo xwin check --locked --target x86_64-pc-windows-msvc
-p arcen-deck-windows --all-targets` on a developer Mac, and natively on
Windows/MSVC with `cargo test --locked -p arcen-deck-windows` and
`cargo build --locked --release -p arcen-deck-windows`, plus the root
shared-crate test and strict Clippy gates. Off Windows the crate compiles to
an empty program. There is no single-platform `--workspace` build.

## Streaming and presentation boundaries

- Production exposes exactly four complete presets: Auto, Speed, Grading,
  and HDR, with the macOS Deck's wording.
- The eight-bit path (`B8G8R8A8`, `G22_P709`) and the ten-bit path
  (`R10G10B10A2`) are separate presenters. Do not force Auto/Speed through
  the ten-bit swapchain or make Grading/HDR depend on the egui surface.
- Transfer characteristics decide HDR. Ten-bit BT.709 Grading stays SDR;
  `G2084_P2020` and HDR10 metadata are set only when the Pier returned PQ.
- Grading/HDR are advertised only when the decoder probe passed on the
  adapter that owns the window's monitor (`HEVC_VLD_MAIN10_444` → `Y410`
  and a real probe keyframe). Otherwise they are refused with a reason,
  never decoded at lower fidelity.
- Measure the effective output depth; an eight-bit result keeps a
  persistent visible warning.
- The decoder never waits for presentation. It decodes on a D3D11 device
  of its own on the root window's adapter, and every window presents on a
  device of its own on that adapter (`decode/shared.rs`,
  `present/handover.rs`): each output picture is copied into a bounded
  pool of NT-handle shared textures (never a shared decoder array) and
  ordered by two shared fences, decode → render before sampling and
  render → decode before a texture is written again. No CPU waits, no
  keyed mutex. Where the shared rule
  `arcen_deck_core::decode::decode_device_split` or the format probe
  refuses (no extended resource sharing or shareable fences, AMD before
  Vega, NVIDIA before 471.11, any other vendor), the decoder and every
  window share the root window's device, as before. This replaces the
  earlier one-device rule (changed 2026-11-18 to remove the contention
  measured in
  [Deck presentation timing](../../docs/architecture/deck-presentation-timing.md));
  established streaming clients decode on a separate device too,
  Moonlight with fences and the same vendor gate. No FFmpeg and no Media
  Foundation decoder in the product.
- Reconnect creates a fresh decoder and waits for a fresh keyframe.
  Teardown discards queued frames.

## Presentation timing and pixel-perfect fullscreen

- **When a frame is shown is the served pipeline's shared rule**
  (`arcen_media::video::PresentTiming`, carried by every
  `PresentableFrame` from the shared media worker;
  [`docs/architecture/deck-presentation-timing.md`](../../docs/architecture/deck-presentation-timing.md)).
  Auto and Speed present each frame at once without tearing (sync interval
  0; the swapchain has no `ALLOW_TEARING`: the flip model shows the newest frame at the next
  vblank or composition and discards an older queued one); no preset tears;
  Grading and HDR
  at sync interval 1, and the ten-bit presenter always. All keep the
  waitable swapchain with frame latency 1. Never add a local Speed rule,
  tearing switch or environment toggle: change the shared contract.
- **Sync interval.** At interval 1 a flip-model present waits for a vsync
  before DWM may compose it: one refresh later than at interval 0, even
  composed (lab, phase-swept: 25.9 → 12.2 ms present to glass). At
  interval 0 a frame whose GPU work finishes late can be replaced before
  it is shown (lab: 0–6 % of frames, depending on phase).
- **Fullscreen draws nothing over the video, for every preset.** The
  connection bar (`render_connection_bar`) opens only when the pointer
  reaches the top-edge strip above it or Ctrl+Alt+Home pins it (keyboard
  to the bar until a command, Esc, or a click into the picture). The
  window input lets winit/egui see the pointer only there
  (`arcen_deck_core::input::overlay_pointer`); everywhere else the host
  has it. With the default Displays mode the stream is the monitor's own
  pixel size, so fullscreen is 1:1.
- **Independent flip.** DWM composes every present unless the video
  swapchain alone covers the monitor: fullscreen, picture 1:1 at the
  origin (stream size = monitor size), opaque, nothing drawn over it. Then
  it may flip it directly (PresentMon "Hardware: Independent Flip") or
  scan it from an overlay plane ("Hardware Composed: Independent Flip");
  whether it does is the driver's and DWM's call (the Windows lab's
  virtual GPU never does: `CheckHardwareCompositionSupport` 0, even a
  textbook fullscreen HWND swapchain is "Composed: Flip"). Keep the
  fullscreen arrangement eligible (`present/flip.rs`). The Deck log names
  both sides: "video arrangement: …", "video presentation timing: …" and
  "video presentation path: …" (DXGI's reading). A frame tears only where
  DWM flips the swapchain directly.

## Input never waits for the UI pass

Pen (`WM_POINTER`), raw mouse and keyboard leave from the window procedure
the moment Windows delivers them, before winit and egui see the message;
the UI pass only publishes routing. Keep pointer events in order at their
own positions, and release held keys on focus loss.

The session window's UI passes begin at most once per refresh of its
display (shared `arcen_deck_core::ui_pacing::PassPacer`, as on the Mac):
egui asks for two more passes after every input pass, and a held pass runs
when due through `repaint_at`. A resize or scale change runs at once. Text
that changes on every pass presents the overlay on every pass, which moves
the video's DWM deadline: pace it (Stream Statistics uses
`ui_pacing::PacedValue`, `STATS_PANEL_REFRESH`).

## Native tablet

The Native option stays visible but greyed, with the shared guidance
strings, until the optional Arcen USB component reports ready, exactly like
the macOS helper. No silent fallback to Tablet support. The service
(`usb/service`) and the prepare tool (`usb/prepare`) link no UI, transport
or media crate (checked by `scripts/ci/check-workspace-boundaries.sh`); no
Arcen kernel driver. The Deck reaches them only through `usb/client`
(`arcen-usb-client`, unprivileged): the component state and actions behind
`DeckServices`, and the pipe client, which accepts the pipe only when the
running `ArcenUsbComponent` service process serves it. The service holds
the tablet only under the shared lease (`component_lease`): it accepts
session-change controls and revokes on a console switch, disconnect,
remote takeover, logoff, the Deck's exit or a stop; a lock keeps it.

## Following a sign-in screen into the desktop

The hand-over is shared behaviour (`arcen_session::login_window_handover`)
and must match the macOS Deck: credentials outlive the hello only for a
sign-in screen, the last frame is held under "Signing in…", and a manual
disconnect is never followed.

## Credentials, settings and logs

No secret is stored: no DPAPI, no Credential Manager. Settings,
`connections.json` and `trusted_pins.json` live in `%APPDATA%\Arcen\Deck\`;
logs in `%LOCALAPPDATA%\Arcen\Deck\Logs\`; `ARCEN_CONFIG_DIR` and
`ARCEN_LOG_DIR` override both. WER crash dumps are minidumps only. A CLI
password file must pass an owner/ACL check before it is read.

## Parallel work

The Windows Deck is built by parallel work streams with strict file
ownership; read the build coordination page before changing anything.
`Cargo.toml`, `src/lib.rs` and `src/main.rs` belong to the `core` stream:
ask there for a dependency or Win32 feature.

Escalate shared API or protocol changes to Shared/Architecture; signing,
installer, privilege (the USB service) and release changes to
Release/Security.
