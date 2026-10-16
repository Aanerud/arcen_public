# Arcen Pier Linux single-file installer

Build with the release Pier binary path in `ARCEN_PIER_BINARY`:

```bash
ARCEN_PIER_BINARY=target/release/arcen-pier \
  cargo build --locked --release -p arcen-pier-linux-installer
```

The resulting `install-arcen-pier` embeds that Pier binary with `include_bytes!`.

That one embedded Pier contains both Linux native capture pipelines:

- eight-bit Auto/Speed through NvFBC → CUDA → NVENC; and
- ten-bit Grading through depth-30 Xorg → XShm → RGB10/P16 conversion → CUDA
  upload → NVENC.

Xorg HDR requests resolve to the Grading pipeline unless `video.desktop_encoding`
declares a Rec.2100 PQ desktop; no separate HDR payload is installed. Rebuild `arcen-pier-linux` before the installer or the single-file
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
