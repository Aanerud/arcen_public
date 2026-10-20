# Wayland qualification: Rocky Linux 10.2, GNOME 49, NVIDIA GRID

**Status:** qualification evidence, 2026-10-17. Not a support claim.

This is the primary qualification target named by the
[Rocky Linux 9 / GNOME 40 baseline](rocky9-gnome40-nvidia-grid.md): an
**unmodified** GNOME 49 on a host with no Xorg server installed, the same
GRID V100D-16Q vGPU profile, and Arcen's own code (the `wayland-provider`
Pier, capenc's Wayland pipelines, the lab harness) running on it.

The host changed driver state during the run, so the evidence is kept in
three labelled stacks and never mixed:

| Stack | Driver | DRM | Compositor renders on | Sections |
| --- | --- | --- | --- | --- |
| **Phase 1 (no DRM)** | 580.65.06, installed `--no-drm` (its `nvidia-drm` does not build on this kernel) | no `/dev/dri` at all | NVIDIA, through EGL surfaceless | [Phase 1](#phase-1-no-drm-nvidia-58065) |
| **Interim** | 580.105.08 with `nvidia-drm`, `modeset=N` | `card0` + `renderD128` (NVIDIA) | **llvmpipe** (Mesa GBM on the render node) | [Interim](#interim-nvidia-drm-loaded-modesetn) |
| **Phase 2** | 580.105.08, licensed, `nvidia-drm modeset=1 fbdev=1` | `card0` + `renderD128` (NVIDIA, no connectors) | **NVIDIA, through NVIDIA GBM** | [Phase 2](#phase-2-drm-capable-nvidia-drm-modeset1) |

The host operator replaced the driver after the phase 1 measurements; the
phase 1 stack no longer exists on this host. Arcen made no driver, module,
kernel, boot, SELinux, GDM or system configuration change.

Verdict vocabulary as in the baseline: **proven** means observed on this
host with the evidence shown; **absent** means checked and not there;
**unknown** means not established and never counts as supported.

## Method

- [`scripts/lab/wayland-probe.sh`](../../../scripts/lab/wayland-probe.sh)
  (read-only) for versions, binaries, D-Bus strings and driver state.
- Raw compositor runs as a throwaway local user with no logind session
  (`systemd-run --uid=… dbus-run-session`), a private `XDG_RUNTIME_DIR`,
  `pipewire` + `wireplumber` with Arcen's video-only override, then
  `mutter` or `gnome-shell --headless --wayland --no-x11 --virtual-monitor
  WxH`, the animated GTK client of `scripts/lab/wayland-capture-lab.py`, and
  a ScreenCast `RecordMonitor` session. Consumers: `pw-dump` (offered
  formats and metadata), `capenc wayland-capture-probe` (debug build;
  delivered rate, upload and NVENC cost), `gst-launch-1.0 pipewiresrc`
  (CPU-only consumer), and throwaway C programs (not committed) for EGL,
  GBM and libei.
- Arcen's own code on the host: `cargo test` for the Pier and capenc, and
  `hosts/linux/examples/wayland_session_harness.rs` through PAM, the real
  session launcher, the Pier's capenc supervision and the Pier's
  `WaylandInjector`, as fresh throwaway local users (`arcenpm*`, never a
  directory account). Debug builds throughout.
- Throwaway users, their homes, processes and logind sessions were removed
  afterwards.

## Versions

| Component | Version |
| --- | --- |
| OS / kernel | Rocky Linux 10.2, `6.12.0-211.66.1.el10_2`, Secure Boot enabled, SELinux enforcing (`selinux-policy-targeted-42.1.18`) |
| systemd / D-Bus | `systemd-257-23.el10_2.2`, `dbus-broker-36` (system), `dbus-daemon-1.14.10` (Arcen's private bus) |
| Mutter / GNOME Shell | `mutter-49.4-4.el10_2` (`libmutter-17`), `gnome-shell-49.4-9.el10_2` |
| PipeWire / session manager | `pipewire-1.4.11`, `wireplumber-0.5.10` |
| Audio server | `pipewire-pulseaudio-1.4.11` (PipeWire-Pulse); no PulseAudio daemon |
| Portals | `xdg-desktop-portal-1.20.0`, `xdg-desktop-portal-gnome-47.3` |
| gnome-remote-desktop | `49.3` |
| libei | `libei-1.5.0` (libei and libeis) |
| Xorg / Xwayland | **no Xorg server**; `xorg-x11-server-Xwayland-24.1.9` |
| Mesa / glvnd | `mesa-libEGL`/`mesa-libgbm` 25.2.7, `libglvnd-egl-1.7.0` |
| GPU | NVIDIA GRID V100D-16Q vGPU at `0000:00:10.0`; driver per stack above |
| First-login apps | `gnome-tour-46.0`, `gnome-initial-setup-46.7` |

## Capability summary (phase 1, no DRM)

| # | Capability | Verdict |
| --- | --- | --- |
| 1 | `--headless`, `--virtual-monitor`, `--wayland`, `--no-x11` on `mutter` and `gnome-shell` | **proven** |
| 1 | Starts with no `/dev/dri` at all, virtual monitor `Meta-0` at the requested mode | **proven** (1080p and 4K) |
| 1 | Compositing on the NVIDIA GPU | **proven**: `Created surfaceless renderer without GPU`, EGL surfaceless on NVIDIA, `GL_RENDERER=GRID V100D-16Q/PCI/SSE2`, Mutter a `G` process in `nvidia-smi`, no llvmpipe threads |
| 1 | DRM render node / GBM / KMS | **absent** (no DRM driver) |
| 2 | `org.gnome.Mutter.ScreenCast` Version 4, `RecordMonitor`/`RecordVirtual`/`RecordWindow`/`RecordArea` | **proven** |
| 2 | Eight-bit `BGRx` and `BGRA` stream formats | **proven** (both offered; `BGRx` negotiated) |
| 2 | Ten-bit (`xRGB_210LE` …) or FP16 stream formats | **absent** |
| 2 | DMA-BUF buffers / DRM modifiers | **absent** (no modifier in any offered format; every buffer `MemFd`) |
| 3 | `SPA_META_VideoDamage` | **proven** (regions per frame; capenc `damage_source=metadata`) |
| 3 | `SPA_META_Header` | **proven** offered |
| 3 | `SPA_META_Cursor` (384×384 bitmap size), `VideoCrop`, `Busy` | **proven** |
| 4 | `org.gnome.Mutter.RemoteDesktop` Version 1, `SupportedDeviceTypes = 7` | **proven** |
| 4 | Arcen's RemoteDesktop D-Bus injection into the headless session | **proven** (harness, see [Arcen on this host](#arcen-on-this-host)) |
| 4 | `ConnectToEIS` on the Mutter RemoteDesktop session | **proven**: connects; seat offers pointer, absolute pointer, keyboard, touch, scroll and button; absolute devices carry a region with the stream's mapping id |
| 4 | Pen / tablet through RemoteDesktop or EIS | **absent**: no tablet method; a client announcing libei `ei_stylus` v1 gets no `ei_stylus` from Mutter 49.4's EIS seat (Arcen's stylus probe, 2026-10-18) |
| 4 | Hard USB tablet into the headless session | **impossible with this session design**: headless Mutter opens no input devices (`META_SEAT_NATIVE_FLAG_NO_LIBINPUT`); refused by the Pier |
| 4 | Portal `ConnectToEIS` | **proven** present (interface file); not exercised |
| 5 | Colour management protocol in Mutter (`wp_color_manager_v1`) and a `bt2100` colour mode | **proven** present in Mutter |
| 5 | A colour mode other than `default` on a virtual monitor | **absent** (`supported-color-modes [default]`; view ColorState sRGB/sRGB, 80 nits reference) |
| 5 | Ten-bit or HDR capture | **absent** |
| 6 | NVIDIA EGL surfaceless display with `EGL_EXT_image_dma_buf_import(_modifiers)` and `EGL_MESA_image_dma_buf_export` | **proven** advertised |
| 6 | CUDA and NVENC (`libcuda`, `libnvidia-encode`), CUDA EGL interop entry points | **proven** (encode measured below) |
| 7 | Per-user PipeWire-Pulse with a monitor source in the headless user's session | **proven** (`auto_null.monitor`) |
| 7 | Arcen's private `WirePlumber` 0.5 override | **proven** honoured |
| 8 | gnome-remote-desktop headless/system mode | **proven** present (`--headless --system --handover`, links FreeRDP); not used by Arcen |

## Phase 1: no DRM, NVIDIA 580.65.06

### 1. Startup and renderer

No `/dev/dri` exists; `nvidia`, `nvidia_uvm` are loaded and `nvidia_modeset`
loads on demand when the compositor opens `/dev/nvidia-modeset`. Mutter 49
starts headless without any DRM device:

```text
libmutter-Message: Running Mutter (using mutter 49.4) as a Wayland display server
libmutter-Message: Created surfaceless renderer without GPU
** Message: Obtained a high priority EGL context
libmutter-Message: Added virtual monitor Meta-0
libmutter-WARNING: Failed to query EGL render node path: One or more argument values are invalid.
RENDER: ColorState for output Meta-0: ClutterColorState 2 (primaries: sRGB, transfer function: sRGB,
        min lum: 0.200000, max lum: 80.000000, ref lum: 80.000000)
RENDER: No direct scanout candidate: no KMS CRTC
```

"Without GPU" means without a DRM device, not without acceleration: the
process holds `/dev/nvidia0`, `/dev/nvidiactl` and `/dev/nvidia-modeset`,
maps `libEGL_nvidia`/`libnvidia-eglcore`, has no llvmpipe threads, and is a
`G` process in `nvidia-smi` (53 MiB). An EGL surfaceless context created as
the same unprivileged user reports:

```text
surfaceless EGL 1.5 vendor=NVIDIA
GL_RENDERER=GRID V100D-16Q/PCI/SSE2   GL_VERSION=OpenGL ES 3.2 NVIDIA 580.65.06
```

`gnome-shell --headless` starts the same way. Outside a logind session it
logs `Unset XDG_SESSION_ID … Asking logind directly`, then fails to register
with GDM (`No display available`) and continues; inside Arcen's own session
it registers its own seatless session (see below). `DisplayConfig` reports
`Meta-0` / `MetaVendor` / `MetaVirtualMonitor`, `layout-mode` physical,
scales `[1.0, 2.0]`, `color-mode` default, `supported-color-modes
[default]`, `rgb-range` auto.

### 2. ScreenCast formats, metadata and rates

`ScreenCast.Version = 4`; the session interface still has `RecordMonitor`,
`RecordWindow`, `RecordArea`, `RecordVirtual`. The `RecordMonitor` port
offers (identical for `mutter` and `gnome-shell`):

```text
EnumFormat BGRx, BGRA   size fixed to the monitor   framerate 0/1   maxFramerate 1..60 (default 60)
Meta       Busy(8) VideoCrop(16) Cursor(589872) Header(32) VideoDamage(16..512)
Buffers    2..16 (default 16), one block, dataType MemFd
```

No format carries a modifier, so DMA-BUF is not offered; every delivered
buffer was shared memory. Damage metadata is new relative to GNOME 40:
capenc reported one or two damage regions per frame of the animated client
and used it (`damage_source=metadata`). Cursor metadata arrived (position
and bitmap serials with pointer motion); no size-0 cursor-only buffers were
seen.

Rates, with the GTK client redrawing every 16 ms:

| Run | Consumer | Delivered | Copy / encode (debug capenc) | Compositor CPU |
| --- | --- | --- | --- | --- |
| `mutter` 1920×1080 | capenc probe, 300 frames, 60 encoded (H.264) | **40.9 fps**, 1 superseded | upload 0.97 ms avg / 1.86 max; NVENC 3.38 ms avg / 7.12 max; 3/3 CUDA read-backs equal | 37% |

That is the only rate measured while the vGPU ran at full capability. The
vGPU was **unlicensed** (`nvidia-gridd` failed certificate validation
against its licence server), and an unlicensed vGPU degrades after a grace
period after the driver loads. Runs from about 20 minutes after boot show
it: `mutter` 4K and `gnome-shell` 1080p captured fine but the host copy took
41–44 ms and an NVENC encode 750–760 ms per frame, so those numbers are not
evidence and are not reported as rates. Then the operator's driver
installation unloaded the NVIDIA modules; Mutter fell back to a Mesa
llvmpipe surfaceless renderer (24 llvmpipe threads, no NVIDIA descriptors)
and still served `BGRx`: a CPU-only consumer saw 52–54 fps at 1080p
(Mutter 351% CPU), 29–32 fps at 4K (512%), and `gnome-shell` 39 fps at
1080p (774%). Those are software-renderer numbers, not this stack's.

**Not established in phase 1:** a 4K rate, a `gnome-shell` rate, sustained
60 fps on NVIDIA, or any harness pipeline run (see Interim).

### 3. Input and EIS

```text
interface org.gnome.Mutter.RemoteDesktop   Version = 1; SupportedDeviceTypes = 7
RemoteDesktop.Session: ConnectToEIS, NotifyKeyboardKeycode, NotifyKeyboardKeysym, NotifyPointerAxis,
  NotifyPointerAxisDiscrete, NotifyPointerButton, NotifyPointerMotionAbsolute,
  NotifyPointerMotionRelative, NotifyTouchDown/Motion/Up, EnableClipboard, DisableClipboard,
  SetSelection, SelectionWrite, SelectionWriteDone, SelectionRead, Start, Stop
```

The clipboard methods were exercised on 2026-10-18 (Mutter 49.4): an
unstarted, clipboard-only RemoteDesktop session carried Deck↔session text and
PNG both ways. `DisableClipboard` cannot be followed by `EnableClipboard`, and
`SelectionOwnerChanged` sends `mime-types` as `(as)` (see the provider
document, § Clipboard).

There is still no tablet or tool method. `ConnectToEIS(a{sv})` on a
RemoteDesktop session linked to a ScreenCast `RecordMonitor` stream returned
a socket; a libei 1.5 sender on it saw:

```text
EI_EVENT_CONNECT
seat "mutter default seat" caps: pointer pointer_absolute keyboard touch scroll button
device "… virtual pointer"                 caps: pointer scroll button
device "… virtual keyboard"                caps: keyboard
device "… shared virtual absolute pointer" caps: pointer_absolute scroll button
       region 0,0 1920x1080 scale=1.00 mapping=<uuid>
device "… shared virtual touch screen"     caps: touch  region 0,0 1920x1080 mapping=<uuid>
emitted absolute motion to 640,360
```

EIS is therefore available on this stack. Arcen injects pointer and
keyboard through RemoteDesktop D-Bus, and uses `ConnectToEIS` only for the
stylus probe. libei 1.5 defines no tablet or pen capability, and Mutter's
EIS server has no stylus device, so pen is unavailable through EIS as well
as through RemoteDesktop. The agent's probe proves it on a live session
(2026-10-18, a throwaway local user, a dev launcher). The session's input
`hello` reported:

```text
"eis_offered":true,"stylus":{"unavailable":"stylus_not_offered","seat_interfaces":["ei_button"]}
pen → {"ok":false,"error":"the session has no stylus device: Tablet support is unavailable in this
       Wayland session: the compositor's emulated-input (EIS) seat offers no stylus (libei 1.7
       ei_stylus, not yet in GNOME). Mouse compatibility is used."}
```

The seat interfaces are those the probe announced (connection, callback,
pingpong, seat, device, button, stylus) and the seat offered: only
`ei_button`. The same probe against libei's own `eis-demo-server` (libei
main `a9bf31da`, the unreleased 1.7) reports `ei_stylus` v1 offered. Pen on
this stack needs a Mutter EIS stylus implementation (none proposed
upstream) on libei ≥ 1.7.0.

Committed text (2026-10-18, Mutter 49.4, a GTK4 text view in the session).
The keyboard is read as a Wayland client only after `wl_seat` advertises
it: asking the headless seat for its keyboard earlier crashed gnome-shell
(`seat_get_keyboard` → `send_keymap`). Mutter's keymap names its symbols
`(unnamed)` and carries the system layout (`English (UK)`) as a group after
the user's sources, so the active group is matched by layout name.
`NotifyKeyboardKeysym` types levels 0–2 of the active group; what the
session proved and refused:

```text
layout  delivered                       refused (reason compositor_lacks_text_input)
no      æøå ÆØÅ ß abc                   ü é ê 😀 日本語
us      abc                             æøå ÆØÅ ü ß é ê 😀 日本語
de      æ ø ü ß abc (AltGr = level 2)   å Æ Ø Å (level 3 or absent) é ê 😀 日本語
```

The text view received exactly the delivered characters; switching the
input source in the running session sent the Deck a new layout. Mutter's
EIS negotiates no `ei_text`, so nothing outside the layout reaches the
session until Mutter merges MR !4996. With `numlock-state=true` (GNOME
remembers Num Lock by default) the session starts with Num Lock on.

### 4. Colour and HDR

`libmutter-17` contains `wp_color_manager_v1`, `bt2100`, `color-mode` and
`supported-color-modes` (`xx_color_manager_v4` is gone), so Mutter 49 has
colour management and HDR output modes. They do not reach a headless
virtual monitor: `supported-color-modes` is `[default]`, the output's
ColorState is sRGB at an 80-nit reference, and ScreenCast offers only
eight-bit `BGRx`/`BGRA` in shared memory. This matches defect 1 of the
Mutter 50 [spike](../linux-wayland-provider.md#headless-hdr-spike-result-2026-09-27)
(virtual outputs declare no colour spaces or HDR transfer functions), and
the shared-memory painting defects (3, 4 there) would still apply to a
ten-bit shared-memory format.

### 5. Audio

PipeWire-Pulse replaces PulseAudio. In Arcen's session the environment's
`PULSE_RUNTIME_PATH=/run/user/<uid>/pulse` reaches the user manager's
socket-activated `pipewire-pulse`, which offers
`auto_null.monitor … PipeWire float32le 2ch 48000Hz`. The private
WirePlumber (video only) runs beside the user manager's own WirePlumber
(audio) without conflict. `pactl` comes from `pulseaudio-utils`, which is
not installed by default (the harness needs it).

### 6. WirePlumber 0.5 override

Arcen's private configuration (distribution files linked one by one plus
`wireplumber.conf.d/99-arcen-video-only.conf`) is honoured: with the
fragment WirePlumber 0.5.10 loads 60 components and none of
`monitor.alsa`, `monitor.alsa-midi`, `monitor.bluez*`, `monitor.libcamera`,
`monitor.v4l2` or their hooks; without it, 83 components including all of
them. WirePlumber stayed up and the ScreenCast stream linked.

## Arcen on this host

`cargo test --locked -p arcen-pier-linux --features wayland-provider`:
779 passed, 1 ignored. `cargo test --locked -p arcen-capenc --features
nvenc,software-h264,wayland`: 243 passed. Build dependencies came from the
distribution (`clang-devel`, `pam-devel`, `pulseaudio-libs-devel`,
`pipewire-devel`, `cmake`; `cmake` is needed by the vendored Opus build).

### Rocky 10 breakage found

1. **SELinux: the launcher cannot exec after PAM opens the session when it
   runs as `unconfined_service_t`. Fixed (`wlp-selinux`), see
   [Linux Pier under SELinux](../../security/linux-selinux.md).**
   `/etc/pam.d/login` runs `pam_selinux open`, which sets the launcher's
   exec context to the user's (`unconfined_u:unconfined_r:unconfined_t:s0`).
   A launcher started by a systemd service
   (`system_u:system_r:unconfined_service_t:s0`, which is how the packaged
   Pier runs) was then refused every `execve` (`EACCES`), so its `loginctl`
   checks failed and the open ended with `pam_systemd did not create the
   required logind session`, although logind did create a correct session
   (`Type=wayland Class=user Seat= Leader=<launcher>`). The denial *is*
   audited (`avc: denied { transition } … tcontext=unconfined_u:unconfined_r:unconfined_t:s0
   tclass=process`); the earlier "no AVC" reading came from running
   `ausearch` over SSH, where it reads standard input unless given
   `--input-logs`. The lab's earlier `systemd-run /bin/bash -c …` wrapper
   also ran as `initrc_t`, not `unconfined_service_t`. The launcher now
   keeps PAM's exec context only for the session user's processes, and the
   installer loads a one-rule SELinux module that lets
   `unconfined_service_t` enter `unconfined_t`; the harness passes as a
   transient `unconfined_service_t` service on this host with `dontaudit`
   disabled and no AVC. The Xorg launcher shares the fix.
2. **Provider selection refuses a host with no DRM render node.** Start-time
   selection requires `render_node_access`; on the phase 1 stack it is
   `Missing`, although Auto/Speed run on the surfaceless NVIDIA renderer
   and a labelled host copy, which needs no render node. Selection would
   refuse Wayland here even with qualified capture-route and input facts.
   Whether `render_node_access` stays a hard gate for host-copy routes is a
   shared policy question for Shared/Architecture.
3. **Lab harness, fixed:** with `--pipeline` the harness closed the overview
   through D-Bus before capenc ran, which made the input stage's first key a
   focus-entering key whose press the client never sees. Fixed and
   re-verified (first and later login).
4. **Lab scripts, fixed:** `wayland-capture-probe.sh` wrote only the 0.4 Lua
   WirePlumber override (0.5 ignores it); `wayland-probe.sh` gained licence,
   module, DRM-node and SELinux facts.
5. **capenc's DMA-BUF import route fails on this driver** (phase 2):
   `cuGraphicsEGLRegisterImage` refuses EGL images of NVIDIA block-linear
   DMA-BUFs. A GL blit into a CUDA-registered texture works (see
   [CUDA import](#cuda-import-of-a-compositor-dma-buf)); reported for
   `wlp-import`, not changed here.
6. A transient `systemd-run --pipe` whose standard output was redirected to
   a regular file twice made PID 1 drop its system-bus connection (`Got
   disconnect on API bus`), after which logind could not start session
   scopes for about three minutes. Seen with lab invocations only; the lab
   scripts now pipe through `cat`. Not an Arcen code path.

GNOME Shell 49 differences that affect a remote desktop:

- **First login:** two Escapes were needed before `OverviewActive` became
  false, as with GNOME 40's welcome dialog (gnome-tour is installed) over
  the Activities overview; later logins start in the overview and need
  one. No screenshot was captured, so the dialog itself is inferred.
- A newly mapped client is not focused until the overview is left, so the
  test client prints `MAPPED` late and the harness's informational
  `input_client` step reports FAIL; every input step after dismissal passes.
- An idle stream delivers nothing after the first frame (as on GNOME 40);
  Keel's one-second keepalive carries the session.

## Interim: nvidia-drm loaded, `modeset=N`

After the operator installed 580.105.08 with `nvidia-drm` (`modeset=N`,
`fbdev=Y`), `renderD128` and `card0` belong to the NVIDIA device. Mutter 49
now adopts the render node, but through **Mesa's** GBM backend, so it
composites in software:

```text
libmutter-Message: Added device '/dev/dri/renderD128' (nvidia-drm) using no mode setting.
libmutter-Message: Failed to initialize accelerated iGPU/dGPU framebuffer sharing: Not hardware accelerated
```

The process holds `renderD128` plus the NVIDIA nodes and runs 24 llvmpipe
threads. A GBM device on `renderD128` as the same user gets backend `drm`,
fails every buffer allocation (`DRM_IOCTL_MODE_CREATE_DUMB: Permission
denied` for `XR24`, `AR24`, `XR30`, `XB30`, `AB4H`, `XB4H`), and its EGL
display is `Mesa Project`, `GL_RENDERER=llvmpipe (LLVM 21.1.8, 256 bits)`.
ScreenCast still offers only `BGRx`/`BGRA` without modifiers. This is
strictly worse than phase 1 for compositing: loading `nvidia-drm` without
`modeset=1` moves Mutter from NVIDIA surfaceless EGL to llvmpipe.

capenc on this stack (the vGPU was inside its unlicensed grace period or
licensed, so encode numbers are not degraded): `mutter` 1080p 46.7 fps delivered, upload 1.17 ms, NVENC
H.264 2.88 ms, Mutter 314% CPU; `mutter` 4K 28.5 fps, upload 3.78 ms, NVENC
HEVC 14.1 ms, Mutter 495% CPU. The session agent now reports the
compositor's render node and GPU (`renderD128`, `0000:00:10.0`, vendor
`0x10de`) and pins the encoder to it; provider selection drops
`render_node_access` from its blockers.

### Harness end to end (interim stack, `gnome-shell`, 1920×1080)

| Step | Result |
| --- | --- |
| Session | seatless logind session `Type=wayland Class=user Seat=`, `gnome-shell` ready in 1.4–1.7 s, `Meta-0` 1920×1080@60, node from `PipeWireStreamAdded` |
| Hand-off | capenc parsed the session's node, output, generation, size, `cursor_mode=embedded`, encoder/compositor GPU `0000:00:10.0` via `renderD128`; 30 frames host-copy, 10 NVENC AUs, read-back verified |
| Audio | `auto_null.monitor` on PipeWire-Pulse |
| Input | Pier `WaylandInjector`: first key, pointer and drag, Shift, client key repeat, wheel and smooth scroll, held-key release on lane loss / injector drop / supersession / disconnect, injector loss and reconnect, refusals (foreign output, stale generation, out of bounds, foreign peer): all pass |
| Teardown | logind session, scope, runtime directory and mounts gone; user manager stopped |

Pipelines (product capenc through `media::capenc::spawn`, debug build;
10 s moving, 10 s idle, keyframe requested while idle). A qualified rate
reaches capenc only as its argv `capture-rate=<fps>`; the product Pier
passes none, so the lab used a `--capenc-run` wrapper that appends it:

| Request | `ADMISSION` | Encoded while moving | Source / cost | Idle | Keyframe |
| --- | --- | --- | --- | --- | --- |
| Auto (H.264) | admit, Auto 30 fps, `copy_path=host-copy`, `colour=assumed`, cursor composited | 30.1 fps, 202 kbps | — | 1.00 AU/s (keepalive) | 25 ms |
| Speed (HEVC), no rate | **downgrade** to Auto 30 fps (`fps_clamped`) | 30.1 fps, 360 kbps | source 46–50 fps, ~20 superseded/s; upload 1.1–1.3 ms; NVENC 4.3–4.5 ms | 1.00 AU/s | 25 ms |
| Speed, `capture-rate=49` (measured) | admit Speed at **49 fps**, `capture_rate_below_contract`, `fps_clamped` | 49.1 fps, 728 kbps | source 49 fps, upload 1.04 ms, NVENC 4.06 ms | 1.00 AU/s | 21 ms |
| Speed, `capture-rate=60` (asserted, not measured) | admit Speed 60 fps | 60.1 fps (49 activity + ~11 NVENC flush submissions per second) | source 49 fps | 1.00 AU/s | 24 ms |

Every stream decodes (`ffprobe`): H.264 High / HEVC Main, `yuv420p`, `tv`
range, BT.709 primaries, transfer and matrix, 2 I-frames and the rest P;
the composited cursor is visible in decoded frames. The last row shows why
`capture-rate` must be a measurement: capenc trusts it and serves the
asserted rate from a slower source. The source here is llvmpipe, so these
numbers say nothing about phase 1's NVIDIA renderer.

## Phase 2: DRM-capable (`nvidia-drm modeset=1`)

Driver 580.105.08, vGPU licensed, `nvidia_drm modeset=Y fbdev=Y`,
`/dev/dri/card0` (`crw-rw---- root:video`) and `/dev/dri/renderD128`
(`crw-rw-rw- root:render`), both NVIDIA `0000:00:10.0`, no KMS connectors,
Secure Boot on. Arcen's session is still seatless and gets no `card0`
access; it needs none.

### Renderer and GBM

```text
libmutter-Message: Added device '/dev/dri/renderD128' (nvidia-drm) using no mode setting.
libmutter-Message: Created gbm renderer for '/dev/dri/renderD128'
libmutter-Message: Boot VGA GPU /dev/dri/renderD128 selected as primary
```

Mutter holds `renderD128` and the NVIDIA nodes, has no llvmpipe threads and
uses 26–47% of one core while streaming (phase 1: 37%; interim llvmpipe:
314–495%). As the same unprivileged user, a GBM device on `renderD128` now
gets backend `nvidia`; `XR24`, `AR24`, `XB30`, `AB4H` and `XB4H` rendering
buffers allocate with block-linear modifier `0x0300000000cdb014` (`XR30`
is not supported), and the GBM EGL display is NVIDIA,
`GL_RENDERER=GRID V100D-16Q/PCI/SSE2`. In Arcen's session `gnome-shell`
holds `renderD128` too (the agent's descriptor evidence); its renderer
string was not captured separately.

### ScreenCast

`RecordMonitor` now offers `BGRx` and `BGRA` twice: with a list of NVIDIA
block-linear modifiers (`0x03000000004fe010`–`…015`,
`0x0300000000cdb010`–`…015`, …) and without (shared memory). No ten-bit or
FP16 format is offered. Metadata adds `SyncTimeline` (explicit sync) to
`Busy`, `VideoCrop`, `Cursor`, `Header` and `VideoDamage`. A consumer that
offers DMA-BUF gets it: every buffer `dmabuf`, modifier
`0x0300000000cdb014`, GPU `same` (compositor and encoder on
`0000:00:10.0`). capenc's `negotiated` log line prints `modifier=0x0` for
that stream while each frame reports `0x0300000000cdb014`; the per-frame
value is the buffer's.

The virtual monitor still reports `supported-color-modes [default]`, so
Grading and HDR are unchanged by the driver.

### CUDA import of a compositor DMA-BUF

capenc's DMA-BUF route (`eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` on the
EGL device for `renderD128`, then `cuGraphicsEGLRegisterImage`) imported
**no** frame of a real Mutter stream: the EGL image is created, and
`cuGraphicsEGLRegisterImage` returns `CUDA_ERROR_INVALID_VALUE` (1) on every
frame, so nothing is encoded and the route falls back nowhere (the probe
reports `import_error`; the product pipelines do not offer DMA-BUF). A
standalone test on a GBM-allocated `XR24` buffer with the same modifier
reproduced it on both the EGL device and the GBM display, and found a route
that works:

| Step | Result |
| --- | --- |
| `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)`, block-linear `XR24` | ok |
| `cuGraphicsEGLRegisterImage(image)` | `CUDA_ERROR_INVALID_VALUE` |
| image → `GL_TEXTURE_2D` (`glEGLImageTargetTexture2DOES`); FBO on it | complete; rendering into the DMA-BUF works |
| `cuGraphicsGLRegisterImage` on that texture | `CUDA_ERROR_INVALID_VALUE` |
| `glBlitFramebuffer` from it into an ordinary `GL_RGBA8` texture registered with `cuGraphicsGLRegisterImage` | ok |
| map, `cuGraphicsSubResourceGetMappedArray`, `cuMemcpy2D` to device memory, read back | ok; the colour cleared into the DMA-BUF read back exactly (`0x40 0x7f 0xbf 0xff`) |

So a same-GPU `gpu-convert` import is feasible on this driver through a GL
blit into a CUDA-registered texture, not through CUDA's EGL image
registration. It was proven on a GBM buffer with the producer's format and
modifier, not yet on a Mutter buffer, and synchronisation (`SyncTimeline`,
implicit fences) was not exercised. Changing capenc's route is
`wlp-import` work.

**Update (`wlp-gpu-speed`):** capenc now implements that route
(`gpu-convert`, with `PipeWire` explicit sync) and proved it on real Mutter
and GNOME Shell buffers on this stack: GPU read-back equal to the
shared-memory frame of the same desktop at 1080p, 1800x1130 and 4K (0
differing pixels), every buffer explicitly synchronised, 2.3–2.9 ms of
import per frame, Mutter's CPU halved against the host copy. See
[GPU import and the paced Speed output](../linux-wayland-provider.md#gpu-import-and-the-paced-speed-output-wlp-gpu-speed).

### Rates

GTK client redrawing every 16 ms; debug capenc; 600 frames per run.

| Virtual monitor | Requested max | Memory | Delivered | Upload / NVENC |
| --- | --- | --- | --- | --- |
| 1920×1080 @60 | 60 | shared memory (host copy) | 39.2 fps | 0.86 ms / 3.38 ms (H.264) |
| 3840×2160 @60 | 60 | shared memory | 40.6 fps | 3.60 ms / 14.6 ms (HEVC) |
| 1920×1080 @60 | 60 | DMA-BUF (no import) | 36.4 fps | — |
| 3840×2160 @60 | 60 | DMA-BUF (no import) | 36.6 fps | — |
| 1920×1080 @60 | 120 (clamped to 60) | DMA-BUF | 38.7 fps | — |
| 1920×1080 @120 | 60 | DMA-BUF | 46.7 fps | — |
| 1920×1080 @120 | 120 | DMA-BUF | **60.7 fps** | — |
| 3840×2160 @120 | 120 | DMA-BUF | **60.3 fps** | — |
| 1920×1080 @120 | 120 | shared memory | **59.6 fps** | 0.89 ms / 2.90 ms (H.264) |
| 3840×2160 @120 | 120 | shared memory | **56.3 fps** (31 frames superseded by the slower debug consumer) | 3.98 ms / 14.3 ms (HEVC) |

The 36–41 fps ceiling at 60 Hz is independent of resolution, memory type,
renderer (phase 1 measured 40.9 fps on the surfaceless renderer) and client
(a `videotestsrc` client at 60 fps gave the same). It lifts when the
virtual monitor runs at 120 Hz and the consumer's maximum framerate is
above 60. Mutter's ScreenCast offers a maximum framerate up to the
monitor's refresh rate; asked for exactly the refresh rate it delivered
about two thirds of the frames, asked for twice the served rate on a
120 Hz monitor it kept up with the 60 fps client at 1080p and 4K in both
memory types. (That this is Mutter's frame pacing is an inference from
these runs, not read from its source.)

Arcen today requests the Deck display's refresh (normally 60 Hz) for the
virtual monitor and capenc offers Speed's 60 fps ceiling, so Arcen sees the
~40 fps source; the harness confirmed it: a Speed request on `gnome-shell`
1080p@60 was downgraded to Auto 30 fps. Serving Speed at 60 on this stack
needs a session/capenc change (a virtual monitor above 60 Hz and an offer
above the served rate, with capenc pacing to 60), then a measured
`capture-rate` on that route. That is a design decision for `wlp-session` /
`wlp-auto-speed`, not done here.

**Update (`wlp-gpu-speed`):** done: Speed offers twice its ceiling, and a
session created for Speed gets a 120 Hz output when the host record
qualifies it. capenc serves Speed at 60–61 fps on a 120 Hz output on both
routes and both compositors in the lab; the product qualification on this
host measured 50–54 fps of 60 Hz pointer motion through the session's
injector, so Speed stays served as Auto here until a qualification
measures 57 fps.

### Deck-drawn cursor (`wp-cursor`, 2026-10-18)

A development Pier on a non-production port, with its own qualification
record, served a throwaway account to a current Deck (`media-smoke` and
`multi-monitor-smoke` with `ARCEN_MEDIA_SMOKE_CURSOR=1`). Mechanism, see
[Cursor](../linux-wayland-provider.md#cursor).

- **Qualification.** `cursor_metadata: metadata=true sprite bitmap
  24x24/fmt11` on a short second desktop; the fact is `available` (also on
  GNOME 40, see [rocky9-gnome40-nvidia-grid.md](rocky9-gnome40-nvidia-grid.md)).
- **Authority.** The Pier logged `active=Local cursor_images=true
  stream_cursor_mode=metadata` for a local-cursor Deck. A second attachment
  that asked for a host cursor (`--cursor-mode host`) to the same desktop
  got `requested=Host active=Local` and the remembered sprite without any
  shape change, so the desktop's cursor outlives its attachments.
- **Shapes.** A GTK 4 window whose regions set `text`, `none` and `pointer`
  produced the arrow (16x24, hotspot 1,1, a 691-byte record with the PNG
  inline), the I-beam (13x23, hotspot 6,11) and the hand (22x21, hotspot
  7,2). Pointer motion and shape changes made no frames (`source_fps=0`
  apart from the 1 fps keepalive): metadata mode produces cursor-only
  buffers.
- **Hidden.** Mutter 49.4 reports a client's `none` cursor as cursor id 0,
  not as an empty sprite. The Deck receives `cursor=hidden` (a 36-byte
  record).
- **Two outputs** (1920x1080 + 1600x900). Each output's capture reported
  its own stream. On one crossing, the output left reported 5 ms before
  the output entered, which is why a multi-output desktop holds a "no
  output" hidden for 80 ms. On another crossing they reported in the
  opposite order. The Deck reused the cached image (a 36-byte record for
  `output1`) and never received a hidden record.

- **OpenH264 epoch** (`--encoder software-h264`) on a metadata desktop:
  the capenc epoch summary reports `served=software backend=openh264-sw-h264
  emitted=34 … cursor_events=4`, and the Pier never logged `invalid
  framed-v2`. The Deck decoded the stream and received the arrow, the I-beam
  and `hidden`.

Deck-side timings in the smoke logs are over the Norway→London VPN link
and are not product numbers.

## Per-preset verdict

| Preset | Phase 1 (no DRM) | Interim (`modeset=N`) | Phase 2 (`modeset=1`) |
| --- | --- | --- | --- |
| **Auto** | **qualified-candidate**: eight-bit `BGRx` host copy on an NVIDIA-composited desktop; 41 fps delivered at 1080p against a 30 fps contract, 0.97 ms upload, 3.4 ms NVENC; damage metadata. Harness pipeline not run on this stack. | **qualified-candidate** (llvmpipe compositor): served end to end at 30 fps through the Pier's supervision | **qualified-candidate**: GPU compositor, host copy, 39–41 fps at 1080p and 4K; harness served Auto 30 fps end to end |
| **Speed** | **unavailable**: 41 fps at 1080p@60 (the 60 Hz pacing ceiling); served as Auto. 120 Hz not tried before the driver changed. | **unavailable** at 60: source 46–50 fps; served only at a measured `capture-rate` below the contract (49 fps) | **unavailable with today's code** (60 Hz monitor: ~40 fps, served as Auto). **qualified-candidate route identified**: a 120 Hz virtual monitor with an offer above 60 delivers 59.6 fps (1080p) / 56.3 fps (4K) by host copy and ~60 fps by DMA-BUF; needs the session change, a measured `capture-rate`, and a Deck run. DMA-BUF import needs the GL-blit route. |
| **Grading** | **unavailable**: no ten-bit or FP16 format; virtual monitor has only the default colour mode | **unavailable** | **unavailable**: same formats and colour mode; the driver does not change it |
| **HDR** | **unavailable**: no HDR colour mode on a virtual monitor and no wide format; with no genuine Grading path, report unavailable, not Grading | **unavailable** | **unavailable** (as phase 1) |

| Input | Verdict |
| --- | --- |
| Keyboard, pointer, scroll, touch through Mutter RemoteDesktop D-Bus and the Pier's injector | **qualified-candidate** (harness on the interim stack: every input step; the D-Bus API is identical in all three stacks) |
| libei / EIS | **available** in the compositor (seat and devices proven in phase 1); Arcen uses it for the stylus probe only |
| Pen / tablet | **unavailable**: no RemoteDesktop tablet method, and the EIS seat offers no `ei_stylus` (probed on a live session); needs Mutter EIS stylus support on libei ≥ 1.7.0 |
| Native tablet (Hard USB) | **refused** in a Wayland session: the headless compositor opens no input devices and the device would reach `seat0` |

## What changes with a DRM-capable NVIDIA driver

Measured above, phase 1 against phase 2:

| | No DRM (phase 1) | `modeset=N` (interim) | `modeset=1` (phase 2) |
| --- | --- | --- | --- |
| Render node | none | `renderD128` | `renderD128` |
| Mutter renderer | NVIDIA, EGL surfaceless | **llvmpipe** via Mesa GBM | NVIDIA via NVIDIA GBM |
| Compositor CPU (1080p, moving) | 37% | 314% | 26–47% |
| ScreenCast memory | shared memory only | shared memory only | DMA-BUF (block-linear modifiers) or shared memory |
| Explicit sync metadata | no | no | `SyncTimeline` |
| CUDA import of the producer buffer | n/a | n/a | feasible through a GL blit; capenc's EGL-image route fails |
| Ten-bit / HDR formats | none | none | none |
| Provider-selection `render_node_access` | Missing | Available | Available |

Loading `nvidia-drm` without `modeset=1` is the worst of the three for a
headless Mutter: it switches to the render node but renders in software.
An installer must treat `modeset=1` as the requirement, not the presence of
`nvidia-drm`. The Speed rate ceiling at 60 Hz is a Mutter pacing property
and does not depend on the driver.
