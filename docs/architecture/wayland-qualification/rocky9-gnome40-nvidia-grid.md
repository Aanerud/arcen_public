# Wayland baseline: Rocky Linux 9.7, GNOME 40, NVIDIA GRID

**Status:** baseline qualification evidence, 2026-10-17. Not a support
claim and not the primary qualification target.

This records what an **unmodified** GNOME 40 stack on an NVIDIA GRID vGPU
can and cannot do for the planned headless Wayland provider. It is a
baseline on an older stack that also runs the production Xorg Pier. The
primary qualification target is an unmodified newer GNOME on a host with no
Xorg server installed (for example RHEL/Rocky Linux 10, whose GNOME has
libei, colour management and a newer PipeWire). No such host was available
for this run.

The earlier headless HDR spike in
[`../linux-wayland-provider.md`](../linux-wayland-provider.md) used a
privately modified Mutter 50.5 in a container with `nvidia-drm modeset=1`.
It is not evidence for this stack, and this stack is not evidence for that
one.

Verdict vocabulary: **proven** means observed on this host with the evidence
shown; **absent** means checked and not there; **unknown** means not
established and never counts as supported.

## Method

- Read-only inventory of packages, binaries, D-Bus interface strings,
  driver files and DRM state. The same facts can be gathered with
  [`scripts/lab/wayland-probe.sh`](../../../scripts/lab/wayland-probe.sh),
  which changes nothing.
- Live experiments ran only as a temporary unprivileged local user with no
  `video` group or seat ACL, inside its own `dbus-run-session` and a private
  `XDG_RUNTIME_DIR`: PipeWire 1.0.1 + WirePlumber, then either
  `mutter --wayland --headless --no-x11 --virtual-monitor 1920x1080` or
  `gnome-shell` with the same flags. A GTK 3 Wayland client drew animated
  content and logged every input event it received. Throwaway C programs
  (not committed) consumed the PipeWire stream and queried EGL, GBM, CUDA
  and Vulkan.
- The production Pier, GDM, Xorg configuration, kernel parameters and driver
  were not touched. The test user, its files and its processes were removed
  afterwards; package set, `dnf history`, `/etc/arcen`, `/etc/X11` and
  `/etc/gdm` checksums were identical before and after, and the Pier stayed
  active throughout.

## Versions

| Component | Version |
| --- | --- |
| OS / kernel | Rocky Linux 9.7, `5.14.0-611.55.1.el9_7`, Secure Boot enabled |
| Mutter / GNOME Shell | `mutter-40.9-29.el9_7`, `gnome-shell-40.10-31.el9_7` (`libmutter-8`) |
| PipeWire / session manager | `pipewire-1.0.1-1.el9`, `wireplumber-0.4.14` |
| Audio server | `pulseaudio-15.0-3.el9`; `pipewire-pulseaudio` not installed |
| Portals | `xdg-desktop-portal-1.12.6`, `xdg-desktop-portal-gnome-41.2` |
| gnome-remote-desktop | `40.0-11.el9_6` |
| libei / libeis | not installed |
| Xorg / Xwayland | `xorg-x11-server-Xorg-1.20.11`, `xorg-x11-server-Xwayland-23.2.7` |
| Mesa / glvnd | `mesa-libEGL`/`mesa-libgbm` 25.0.7, `libglvnd-egl-1.3.4` |
| GPU | NVIDIA GRID V100D-16Q vGPU, driver 580.65.06 (runfile install), CUDA driver API 13000 |
| `nvidia_drm` | `modeset=N`, `fbdev=Y` |
| GDM policy | `WaylandEnable=false` (host is configured for the Xorg Pier) |

## Capability summary

| # | Capability | Verdict |
| --- | --- | --- |
| 1 | `--headless` and `--virtual-monitor WxH[@R]` on `mutter` and `gnome-shell` | **proven** |
| 1 | Starts with no display connector, virtual monitor `Meta-0` at 1920x1080@60 | **proven** |
| 1 | vGPU exposed as a DRM render node (`renderD128`, `nvidia-drm`) | **proven** |
| 1 | KMS on `card0` | **absent** (`modeset=N`, zero connectors) |
| 1 | Hardware GBM renderer in headless Mutter | **absent** on this configuration; surfaceless renderer used instead |
| 1 | Compositing on the NVIDIA GPU rather than llvmpipe | **proven** by process evidence; GL renderer string not captured |
| 2 | `org.gnome.Mutter.ScreenCast` v4 with `RecordMonitor`, `RecordVirtual`, `RecordWindow`, `RecordArea` | **proven** |
| 2 | Eight-bit `BGRx` stream | **proven** |
| 2 | `BGRA`, ten-bit or FP16 stream formats | **absent** |
| 2 | DMA-BUF buffers / DRM modifiers | **absent** (Mutter disables DMA-BUF when surfaceless) |
| 2 | Shared-memory (`MemFd`) buffers | **proven** |
| 3 | `SPA_META_Cursor` with `cursor-mode=2` (metadata) | **proven** |
| 3 | `SPA_META_VideoCrop` | **proven** |
| 3 | `SPA_META_VideoDamage` | **absent** |
| 3 | `SPA_META_Header` | **absent** |
| 4 | `org.gnome.Mutter.RemoteDesktop` v1, device types keyboard, pointer, touchscreen | **proven** |
| 4 | Injection into the headless session: absolute pointer, buttons, smooth and discrete scroll, keysym, keycode, touch | **proven** |
| 4 | `ConnectToEIS` / libei (Mutter or portal) | **absent** |
| 4 | Tablet / pen injection through RemoteDesktop | **absent** |
| 4 | Pen through `uinput` into a headless Mutter | **absent by design**: headless Mutter 40 runs with `META_SEAT_NATIVE_FLAG_NO_LIBINPUT` and opens no input devices (source, `gnome-40` branch); a uinput device would reach `seat0` instead |
| 4 | Clipboard methods on the RemoteDesktop session | **proven** and exercised (2026-10-18: Deck↔session text and PNG through an unstarted clipboard-only session; see the provider document, § Clipboard) |
| 5 | Colour management, HDR output, ten-bit composition | **absent** |
| 6 | NVIDIA EGL dma-buf import with modifiers (EGLDevice platform) | **proven** advertised |
| 6 | NVIDIA GBM platform / NVIDIA GBM buffer allocation | **absent** (`modeset=N`) |
| 6 | NVIDIA EGL dma-buf export | **absent** (`EGL_BAD_MATCH`) |
| 6 | Vulkan `VK_EXT_external_memory_dma_buf` on the NVIDIA device | **absent** |
| 6 | CUDA EGL/GL interop and external-memory entry points | **proven** exported |
| 6 | Compositor DMA-BUF → EGL → CUDA import | **unknown** (no DMA-BUF producer on this configuration) |
| 7 | Per-user PulseAudio with a monitor source in a headless user session | **proven** |
| 8 | gnome-remote-desktop headless mode | **absent** |

## Evidence

### 1. Headless startup, DRM devices and GPU use

```text
$ mutter --help
  --headless                 Run as a headless display server
  --virtual-monitor          Add persistent virtual monitor (WxH or WxH@R)
$ gnome-shell --help        # same two options

$ ls -l /dev/dri/by-path
pci-0000:00:10.0-card -> ../card0          # crw-rw----  root video
pci-0000:00:10.0-render -> ../renderD128   # crw-rw-rw-  root render
$ cat /sys/module/nvidia_drm/parameters/modeset
N
$ ls /sys/class/drm
card0  renderD128  version                 # no card0-* connectors
```

Mutter 40 headless looks for a KMS `card` node, not a render node. As an
unprivileged user without seat access it falls back to a surfaceless
renderer and switches off DMA-BUF screen casting:

```text
mutter-WARNING: Failed to open gpu '/dev/dri/card0': Failed to open DRM device: Permission denied
mutter-Message: Created surfaceless renderer without GPU
mutter-Message: Disabling DMA buffer screen sharing (surfaceless)
mutter-Message: Added virtual monitor Meta-0
```

Granting `card0` access was deliberately not tried. Nothing holds DRM master
on `card0`, so the probe would have become master of the GPU the live Xorg
sessions use, and with `modeset=N` there is no KMS for it to drive anyway.

The surfaceless renderer still composites on the NVIDIA GPU. Mutter mapped
`libEGL_nvidia.so.580.65.06` and `libnvidia-eglcore`, was listed by
`nvidia-smi` as a graphics (`G`) process (39 MiB idle, 163 MiB while
streaming; `gnome-shell` 203 MiB), and had no llvmpipe worker threads. Mesa
was also loaded by glvnd but did not render. The GL renderer string was not
captured, so this is process evidence rather than a direct query.

`DisplayConfig.GetCurrentState` reported one monitor
`('Meta-0', 'MetaVendor', 'MetaVirtualMonitor')`, mode `1920x1080@60`,
supported scales `[1.0, 2.0]`, `layout-mode` 2, and no colour or HDR
properties.

`gnome-shell --headless` started and owned `org.gnome.Mutter.ScreenCast`,
`org.gnome.Mutter.RemoteDesktop` and the usual Shell names. It also asked
logind for the *caller's* session and registered it with GDM:

```text
GNOME Shell-Message: Unset XDG_SESSION_ID, getCurrentSessionProxy() called outside a user session. Asking logind directly.
GNOME Shell-Message: Will monitor session <n>
GNOME Shell-Message: Registering session with GDM
```

Here `<n>` was the operator's own SSH session, not a session created for the
compositor. GDM logged nothing and the Pier was unaffected.

### 2. ScreenCast API and stream formats

```text
interface org.gnome.Mutter.ScreenCast         Version = 4; CreateSession(a{sv}) -> o
interface org.gnome.Mutter.ScreenCast.Session Start, Stop, RecordMonitor(s, a{sv}),
                                              RecordWindow(a{sv}), RecordArea(iiii, a{sv}),
                                              RecordVirtual(a{sv}); signal Closed
interface org.gnome.Mutter.ScreenCast.Stream  signal PipeWireStreamAdded(u);
                                              property Parameters a{sv} = {position, size}
```

`cursor-mode`, `is-platform` and `is-recording` are present as option
strings in `libmutter-8.so.0`.

The stream port's parameters are identical under `mutter` and `gnome-shell`.
For `RecordMonitor` on `Meta-0`:

```text
EnumFormat [{"format": "BGRx", "size": {"width": 1920, "height": 1080},
             "framerate": 0/1, "maxFramerate": {"default": 60/1, "min": 1/1, "max": 60/1}}]
Meta       [{"type": "Busy"}, {"type": "VideoCrop", "size": 16}, {"type": "Cursor", "size": 589872}]
Buffers    [{"buffers": {"default": 16, "min": 2, "max": 16}, "blocks": 1,
             "size": 8294400, "stride": 7680, "align": 16}]
```

`RecordVirtual` offers the same single format with a size range from 1x1
to 16384x16386 (default 1280x720) and a maximum framerate up to 1000/1. Unless
the consumer fixes the size, it gets the producer's default, 1280x720.

The probe consumer offered `BGRx`, `BGRA`, `RGBx`, `RGBA`, `xRGB_210LE`,
`ARGB_210LE`, `xBGR_210LE` and `ABGR_210LE`, first with a mandatory DRM
modifier list and then without, and accepted DMA-BUF, MemFd and MemPtr:

```text
NEGOTIATED format=BGRx size=1920x1080 framerate=0/1 max_framerate=60/1 modifier_prop=no
SUMMARY frames=437 fps~36.4 dmabuf=0 memfd=437 corrupted=0 header=0 damage_meta_frames=0
        cursor_valid=364 cursor_bitmap=2
```

Every buffer was shared memory. With an animated GTK client redrawing
roughly every 16 ms, a 1080p `RecordMonitor` stream under plain Mutter
delivered about 36–37 fps; `ps` reported Mutter's average CPU over the run
as 21.7% of one core. Under
`gnome-shell`, with only the cursor moving, it delivered 7 fps: frames are
produced on change. These are single observations, not a rate
qualification. The padding byte of `BGRx` was not `0xff` (`0x0a 0x0a 0x0a
0xcf`), so consumers must ignore it.

### 3. Damage, cursor and header metadata

Mutter 40 offers only `Busy`, `VideoCrop` and `Cursor` metadata. The
consumer asked for `Header`, `VideoDamage`, `Cursor` and `VideoCrop`; the
buffers carried `Busy(8) VideoCrop(16) Cursor(589872)`.

- **Cursor:** 589872 bytes is a 384x384 ARGB bitmap plus headers. A consumer
  whose cursor-meta size range stops below that receives **no cursor
  metadata at all**: the first run, capped at 256x256, got none. With the cap
  raised, the cursor position arrived on every frame and the bitmap only when
  it changed. Cursor-only updates arrive as buffers with `chunk.size == 0`
  that carry no new pixels:

  ```text
  frame 2 type=2 size=0 stride=0 ... cursor=valid
    cursor id=1 pos=300,300 hotspot=0,0 bitmap_offset=28
  ```

- **Damage:** no `VideoDamage`. Keel must derive damage itself or treat each
  delivered frame as full damage (`PendingExternalDamage` unknown → full).
- **Header:** no `Header` metadata, so no producer PTS or sequence number in
  the buffer. Timing must come from `pw_stream` time or arrival.

### 4. Input

```text
interface org.gnome.Mutter.RemoteDesktop   Version = 1; SupportedDeviceTypes = 7
                                           (keyboard | pointer | touchscreen); CreateSession() -> o
interface org.gnome.Mutter.RemoteDesktop.Session
  Start, Stop, NotifyKeyboardKeycode(u,b), NotifyKeyboardKeysym(u,b), NotifyPointerButton(i,b),
  NotifyPointerAxis(d,d,u), NotifyPointerAxisDiscrete(u,i), NotifyPointerMotionRelative(d,d),
  NotifyPointerMotionAbsolute(s,d,d), NotifyTouchDown(s,u,d,d), NotifyTouchMotion(s,u,d,d),
  NotifyTouchUp(u), EnableClipboard, DisableClipboard, SetSelection, SelectionWrite,
  SelectionWriteDone, SelectionRead; properties SessionId, CapsLockState, NumLockState
```

There is no `ConnectToEIS` and no tablet or tool method. The same is true of
the shipped portal interface
(`/usr/share/dbus-1/interfaces/org.freedesktop.portal.RemoteDesktop.xml`).
libei and libeis are not installed.

A RemoteDesktop session linked to a ScreenCast `RecordMonitor` stream (through
`remote-desktop-session-id`) injected into the headless compositor. The GTK
client logged:

```text
motion-notify-event  x=301.0 y=307.0 device=Wayland Pointer   source=mouse
button-press-event   button=1         device=Wayland Pointer   source=mouse
scroll-event                          device=Wayland Finger Scrolling source=touchpad
key-press-event      keyval=0x61 hw=38 device=Wayland Keyboard source=keyboard
touch-event          x=341.0 y=427.0  device=Wayland Touch     source=touchscreen
```

Absolute coordinates are relative to the stream named in the call.
`NotifyPointerAxis` with the finger flag arrives as touchpad scrolling.
`NotifyKeyboardKeycode` takes evdev codes (30 arrived as X keycode 38).
Keyboard events need the target window to have focus.

Mutter 40 exposes `zwp_tablet_manager_v2` to clients, so applications can
receive tablet input. Nothing tested here can inject it: RemoteDesktop has no
tablet device type. A `uinput` tablet is not seen by a headless Mutter:
`meta-backend-native.c` on the `gnome-40` branch sets
`META_SEAT_NATIVE_FLAG_NO_LIBINPUT` when headless, so no libinput context
exists. Such a device would reach whatever runs on `seat0`. Without
`ConnectToEIS` there is no emulated stylus either: pen is unavailable in a
GNOME 40 Wayland session for good, and the hello says `no_emulated_input`.
Hard USB is refused for the same seat reason.

### 5. Colour and HDR

`libmutter-8.so.0` contains no colour-management protocol
(`wp_color_manager_v1` and `xx_color_manager_v4` are absent) and no HDR,
`bt2100` or colour-mode strings. `DisplayConfig` reports no colour properties.
ScreenCast offers `BGRx` only. A ten-bit or HDR capture is not possible on
this stack.

### 6. NVIDIA import facts

EGL client extensions (the glvnd union of NVIDIA and Mesa) include
`EGL_KHR_platform_gbm`, `EGL_MESA_platform_gbm`, `EGL_EXT_platform_device`
and `EGL_MESA_platform_surfaceless`.

A GBM display on `renderD128` does **not** reach NVIDIA with `modeset=N`.
The Mesa `drm` GBM backend is chosen, the EGL vendor is Mesa, and buffer
allocation fails, although `nvidia-drm_gbm.so` and `libnvidia-egl-gbm` are
installed:

```text
KMS: DRM_IOCTL_MODE_CREATE_DUMB failed: Permission denied
gbm device on renderD128: ok backend=drm
GBM platform display: EGL 1.5 vendor=Mesa Project
gbm_bo_create XR30 RENDERING: failed
gbm_bo_create XR24 RENDERING|LINEAR: failed
```

The NVIDIA EGLDevice platform (`render_node=/dev/dri/renderD128`,
`EGL_NV_device_cuda`) supports `EGL_EXT_image_dma_buf_import`,
`EGL_EXT_image_dma_buf_import_modifiers`, `EGL_MESA_image_dma_buf_export`,
`EGL_KHR_surfaceless_context`, `EGL_KHR_no_config_context` and
`EGL_KHR_fence_sync`/`wait_sync`. `EGL_ANDROID_native_fence_sync` is absent.
Its import formats include `XR24 AR24 XB24 AB24 XR30 AR30 XB30 AB30 XB4H
AB4H NV12 P010`:

```text
XR24 modifiers (13): 0x03000000004fe010..015 0x0300000000cdb010..015 0x0000000000000000(ext-only)
XR30 modifiers (13): same set
AB4H modifiers (7):  0x03000000004fe010..015 0x0000000000000000(ext-only)
```

`LINEAR` is external-only for every RGB format, so a linear DMA-BUF can be
sampled only as `GL_TEXTURE_EXTERNAL_OES`. That target cannot be registered
with `cuGraphicsGLRegisterImage`. The CUDA route for linear buffers would
have to be `cuGraphicsEGLRegisterImage` or a GPU blit, and neither was
exercised with a real buffer.

Exporting a DMA-BUF from an NVIDIA GL texture as a stand-in producer was
also refused:

```text
eglExportDMABUFImageQueryMESA: fourcc=AB24 / AB30 / AB4H, modifier=0x0300000000cdb013
eglExportDMABUFImageMESA: EGL_BAD_MATCH (0x3009)
```

The NVIDIA Vulkan device (`GRID V100D-16Q`) exposes
`VK_EXT_image_drm_format_modifier`, `VK_KHR_external_memory_fd`,
`VK_KHR_external_semaphore_fd`, `VK_EXT_queue_family_foreign` and
`VK_KHR_video_encode_h264`/`h265`, but **not**
`VK_EXT_external_memory_dma_buf`.

CUDA works: `cuInit`/`cuDeviceGet` succeed on `GRID V100D-16Q`, and
`libcuda.so.1` exports `cuGraphicsEGLRegisterImage`,
`cuGraphicsResourceGetMappedEglFrame`, `cuGraphicsGLRegisterImage`,
`cuImportExternalMemory` and `cuImportExternalSemaphore`.
`libnvidia-encode.so.1` and `libnvidia-fbc.so.1` load.

NVIDIA's GBM backend requires `nvidia-drm modeset=1`, and the earlier spike
had `modeset=1` set when it got DMA-BUF capture on this vGPU profile. It is
expected, but not verified here, that EGL export and Vulkan DMA-BUF memory
follow the same switch. Changing it means a kernel parameter and a reboot,
outside a read-only baseline. Whether a compositor DMA-BUF imports into CUDA
on this driver therefore remains **unknown**.

### 7. Audio

PulseAudio 15 is the per-user audio server. `pulseaudio.socket` and
`pulseaudio.service` are globally enabled user units. `pipewire.socket` and
`wireplumber.service` are also enabled, for video. `pipewire-pulse` is not
installed. An existing desktop login runs `pulseaudio`, `pipewire` and
`wireplumber` side by side. The VM has no sound card (`aplay: no soundcards
found`). In the headless test user's session `pulseaudio --start` gave:

```text
Server Name: pulseaudio    Server Version: 15.0
Default Sink: auto_null    Default Source: auto_null.monitor
```

`module-always-sink` in `/etc/pulse/default.pa` provides that null sink, so
the existing Linux audiocap path has a monitor source to capture.

### 8. gnome-remote-desktop

`gnome-remote-desktop-daemon` 40.0 accepts only `--version`, `--rdp-port`,
`--vnc-port` and `--max-parallel-connections`. Its binary contains no
`headless` string and does not link FreeRDP. It is a per-user VNC server for
an existing session, with no headless or system mode.

## Per-preset verdict for this stack

| Preset | Verdict | Reason |
| --- | --- | --- |
| **Auto** | **qualified-candidate**, host-memory path only | Genuine eight-bit `BGRx` virtual output. Only shared memory is offered, so the path is a CPU readback in Mutter plus an upload, reported as `capture_zero_copy=false` and `copy_path=host-copy`. 36 fps was observed at 1080p against a 30 fps contract. `wlp-auto-speed` (2026-10-17): served end to end through capenc on the real session hand-off and the Pier's capenc supervision at 30 fps, Keel idle (keepalive only) and keyframe on demand while idle, BT.709 limited 4:2:0 stream truth, composited cursor (see `linux-wayland-provider.md`). Still needs a Deck end to end. |
| **Speed** | **unavailable** | No DMA-BUF, so no zero-readback path; the shared-memory rate is not qualified for 60 fps (36 fps observed; 37.5 average, 41 peak under gnome-shell in `wlp-auto-speed`). A Speed request is admitted by the shared `admit_speed` and visibly served as Auto (`copy_path_not_rate_qualified`), never advertised as Speed. |
| **Grading** | **unavailable** | Composition and capture are eight-bit `BGRx` only. No ten-bit or FP16 format exists. |
| **HDR** | **unavailable** | No colour management or HDR output. There is no genuine Grading path to downgrade to, so HDR requests must be reported as unavailable, not as Grading. |

| Input | Verdict |
| --- | --- |
| Keyboard, pointer, scroll, touch via Mutter RemoteDesktop D-Bus | **qualified-candidate**: proven injection into the headless session, bound to the ScreenCast stream; still needs generation barriers, held-state release and teardown evidence |
| libei / EIS | **unavailable** on GNOME 40 |
| Pen / tablet | **unavailable** through RemoteDesktop; the `uinput` route is **unknown** and is not an automatic fallback |

## Findings that affect the implementation plan

1. **NVIDIA DMA-BUF depends on `nvidia-drm modeset=1`.** With `modeset=N`,
   NVIDIA GBM is unavailable, NVIDIA EGL DMA-BUF export and Vulkan DMA-BUF
   memory are absent, and headless Mutter goes surfaceless and turns off
   DMA-BUF screen casting. `wlp-import` cannot assume DMA-BUF.
   `wlp-installer` must detect `modeset` and explain the change, kernel
   parameter and reboot involved. The earlier spike re-measured the Xorg
   pipelines unchanged with `modeset=1`; that should be repeated on the
   qualification host.
2. **Mutter 40 headless opens KMS `card` nodes.** A hardware-backed headless
   session needs `card0` access, which means a seat or ACL decision for a
   user with no physical seat. Even then, whether Mutter 40 offers DMA-BUF
   from a GBM-backed headless renderer is unknown. The earlier Mutter 50
   spike ran headless on the render node through GBM.
3. **No damage or header metadata on GNOME 40.** Keel must treat each
   delivered frame as full or unknown damage, and timing must come from the
   stream, not buffer metadata.
4. **Cursor metadata needs a large meta size.** The consumer must accept at
   least `CURSOR_META_SIZE(384, 384)`, or it silently gets no cursor
   metadata. It must also handle `chunk.size == 0` cursor-only buffers.
5. **GNOME Shell 40 binds to the caller's logind session and registers it
   with GDM.** The `wlp-session` adapter must launch each compositor in its
   own logind session (PAM with `pam_systemd`). It must never start one from
   the Pier service's session or an operator shell.
6. **Input on GNOME 40 is Mutter D-Bus only.** There is no EIS and no pen. A
   GNOME 40 provider must advertise pen as unavailable, or as an explicitly
   negotiated pointer emulation, never as pressure-capable pen.
7. **GNOME 40 is an SDR8 host-memory platform at best.** Grading and HDR, and
   any zero-copy Speed claim, depend on the newer-GNOME qualification host.
8. **Unexercised links that must be proven on the target host:** compositor
   DMA-BUF → `cuGraphicsEGLRegisterImage`, fence/sync behaviour (no
   `EGL_ANDROID_native_fence_sync` on NVIDIA here), sustained 1080p/4K
   capture rates, and Deck end-to-end evidence.

## Deck-drawn cursor (`wp-cursor`, 2026-10-18)

The second cursor-only buffer above (`bitmap_offset=28`, hotspot 0,0) is not
"no new bitmap": its `spa_meta_bitmap` is all zero, which is Mutter 40's
empty sprite (`meta_screen_cast_stream_src_set_empty_cursor_sprite_metadata`
zeroes it; gnome-remote-desktop reads it as a hidden pointer). A new
headless desktop reports it until the pointer enters a surface. capenc now
reads it as an empty sprite (hidden), and keeps cursor metadata from every
buffer not marked corrupted, because Mutter writes a sprite only when it
changes.

`arcen-pier wayland qualify` on this host (gnome-shell 40.10, PipeWire
1.0.1, NVIDIA 580.65.06; development record, temporary account) proved the
`cursor_metadata` fact on a short desktop recorded with `cursor-mode`
metadata while the pointer moved:

```text
ok   cursor_metadata: metadata=true sprite bitmap 24x24/fmt11; desktop stopped=true gone=true
```

(`fmt11` is `SPA_VIDEO_FORMAT_RGBA`, premultiplied.) No Deck has been served
a Deck-drawn cursor on this host; the end-to-end proof is on the Rocky 10
host.
