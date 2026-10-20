# Arcen Pier Linux single-file installer

Build with the release Pier binary path in `ARCEN_PIER_BINARY`:

```bash
cargo build --locked --release -p arcen-pier-linux --features wayland-provider
ARCEN_PIER_BINARY=target/release/arcen-pier \
  cargo build --locked --release -p arcen-pier-linux-installer
```

Build on the oldest supported host (glibc): the installer test-runs the
embedded Pier before writing anything and refuses a host whose loader cannot
start it, naming the missing library or `GLIBC_` version. With
`wayland-provider` the Pier links `libpipewire-0.3.so.0`, so that library is
a runtime dependency on every host.

The resulting `install-arcen-pier` embeds that Pier binary with `include_bytes!`.

That one embedded Pier contains both Linux native capture pipelines:

- eight-bit Auto/Speed through NvFBC → CUDA → NVENC; and
- ten-bit Grading through depth-30 Xorg → XShm → RGB10/P16 conversion → CUDA
  upload → NVENC.

Xorg HDR requests resolve to the Grading pipeline unless `video.desktop_encoding`
declares a Rec.2100 PQ desktop; no separate HDR payload is installed. A
desktop is ten-bit only when it is created for ten-bit work unless
`video.x11_desktop_depth` is set to `30`, which a host used for grading in
Flame or Resolve should do after installation (see
`docs/operations/pier-administration.md`, "Linux desktop depth"). Rebuild `arcen-pier-linux` before the installer or the single-file
artifact will silently embed an older pipeline implementation.

`/etc/arcen/xorg.conf` ships without a GPU `BusID`. On the real root the
installer reads `/sys/bus/pci/devices`, applies the shared
`arcen_outputs::xorg_gpu` decision, and sets the NVIDIA GPU's `BusID` (backing
up an existing operator file to `xorg.conf.before-<version>`), keeps a correct
one, or warns when the host has no usable NVIDIA GPU. It never fails the
install for this; see "The NVIDIA GPU in `/etc/arcen/xorg.conf`" in
`docs/operations/pier-administration.md`. The sysfs reader is the Pier's own
`hosts/linux/src/display/gpu_pci.rs`, compiled in with `#[path]`. An existing
file is repaired only when it passes the launcher's trust rule
(`hosts/linux/src/display/xorg_template_trust.rs`: opened without following
symlinks, regular, root-owned, not group/other writable); otherwise the
installer warns and leaves it alone. Backups are linked into the first free
`xorg.conf.before-<version>[.N]` name and never replace an existing one.

## Upgrading: host speakers become muted by default

The Linux Pier now honours `audio.local_playback`, whose default is `muted`;
an upgraded host whose `/etc/arcen/pier.json` does not name it silences its
own speakers during every session, and refuses a session when it cannot prove
that. The installer keeps an existing configuration as it is. An operator who
wants the host's speakers to keep playing sets `"local_playback": "audible"`
in the `audio` section (see "Upgrade and rollback" in
`docs/operations/pier-administration.md`).

## Desktop provider

Before writing anything the installer gathers this host's facts
(`src/desktop.rs`: the Xorg server by the Pier's own path list, `--help` and
`--version` of GNOME Shell/Mutter and PipeWire, logind, `dbus-daemon`,
WirePlumber, `/dev/dri/renderD*` with the GPU behind it, the NVIDIA module
and `nvidia_drm modeset`, the kernel's DRM `fb_create` interface, SELinux
mode, and the existing `capture.provider`), and the shared pure policy
`arcen_session::install_provider` decides and words the explanation: X11 when
an Xorg server is installed, Wayland (pending `arcen-pier wayland qualify
--write`, run by the installer; the service starts only if it passes) when
the headless GNOME stack and a usable render node are present, otherwise a
refusal naming what to install. A render node of any vendor qualifies: the
qualification probes for a native encoder (NVENC) first, and a host without
one (Intel or AMD graphics, a GPU with no encoder) is qualified and served
with the software encoder (OpenH264, H.264 4:2:0 at up to 30 fps, outputs
fitted to 1920x1200). It never installs packages or changes
driver, kernel, device-permission or boot configuration, or the SELinux
mode; on an SELinux host it loads only Arcen's own policy module (checked
for `semodule` in preflight, loaded before the qualification runs). `--check` prints the same
summary and writes nothing; every real install appends it, the decision and
the qualification output to `/var/log/arcen/install.log`.

Uninstall removes the qualification record, a leftover temporary
qualification account (GECOS-matched), and closes UDP 18444 only when
`/var/lib/arcen/installer-opened-firewall` records that the installer opened
it. See "What the Linux installer chooses, and why" in
`docs/operations/pier-administration.md`.
