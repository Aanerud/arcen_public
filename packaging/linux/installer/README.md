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

Xorg HDR requests resolve to the Grading pipeline; no separate HDR payload is
installed. Rebuild `arcen-pier-linux` before the installer or the single-file
artifact will silently embed an older pipeline implementation.
