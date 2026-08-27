# Arcen Pier Linux single-file installer

Build with the release Pier binary path in `ARCEN_PIER_BINARY`:

```bash
ARCEN_PIER_BINARY=target/release/arcen-pier \
  cargo build --locked --release -p arcen-pier-linux-installer
```

The resulting `install-arcen-pier` embeds that Pier binary with `include_bytes!`.
