# Linux Host Ownership

**Owner role:** Linux Host

Own Linux capture, input injection, machine authentication integration, host
session lifecycle, and Linux packaging coordination in this path.

Validate on Linux with
`cargo build --locked --release -p arcen-pier-linux`. The Pier embeds capenc,
audiocap, input-helper, session-agent, and session-launcher as multicall
subcommands; release packaging must not ship duplicate helper executables. Also
run the root shared-crate test and strict Clippy gates. There is no
single-platform `--workspace` build. Platform behavior requires Linux
integration coverage.

Building requires `libpam0g-dev` and `libpulse-dev`. NVENC capture requires an
NVIDIA GPU and driver on the build/test host.

## Capture pipeline boundaries

- Eight-bit sessions use NvFBC → CUDA → NVENC. Preserve this as the
  device-to-device fast path; do not route it through XShm or host conversion.
- Every depth above eight uses the separate depth-30 Xorg/MIT-SHM path. Derive
  packed RGB10 ordering from the live visual masks, convert in shared media
  code, then upload once to CUDA/NVENC.
- Xorg depth 30 is genuine precision but not an HDR composition contract. PQ
  and HLG requests must resolve to Grading BT.709 SDR until a color-managed
  Wayland provider proves transfer, primaries, metadata, and capture format.
- XShm cannot composite the host cursor. Resolve Host to Local before native
  preflight and live spawn; keep Host unchanged on the eight-bit NvFBC path.
- READY and session truth must identify `capture=nvfbc
  capture_zero_copy=true` or `capture=xshm capture_zero_copy=false`.

Escalate shared API or protocol changes to Shared/Architecture; authentication,
privilege, GPU, signing, packaging, and release changes to Release/Security.

## Deploying a Pier

Requires systemd and root on the target. Install prefix `/opt/arcen/bin/`,
service `arcen-pier.service`, log `/var/log/arcen/arcen-pier.log`, unified
runtime config `/etc/arcen/pier.json` (the packaged service passes only this
path; common fields match Windows `%ProgramData%\Arcen\pier.json`).

Do not record real hostnames, IP addresses, or SSH runbooks in this repository —
it is public. Use your own inventory for that.

### Build (on the Linux target)

```bash
# One fused Pier binary with all helper subcommands.
cargo build --release -p arcen-pier-linux
```

### Deploy (on the Linux target)

```bash
systemctl stop arcen-pier
cp target/release/arcen-pier /opt/arcen/bin/arcen-pier
systemctl start arcen-pier
systemctl is-active arcen-pier
tail -5 /var/log/arcen/arcen-pier.log
```

After the fused Pier is proven healthy, obsolete standalone helper files from
older deployments may be removed from `/opt/arcen/bin/`.

### Multicall release invariant

- Build and package only `target/release/arcen-pier`.
- Helper isolation remains process-based through `current_exe()` subcommands;
  fusion does not move capture, audio, input, or session work in-process.
- Legacy `--capenc-bin`, `--audiocap-bin`, and equivalent JSON paths are
  accepted only for upgrade compatibility, warned about, and ignored.
- During a rollback-safe upgrade, retain old standalone helper files until the
  fused Pier has authenticated, streamed, accepted input, and restarted cleanly.
- Remove obsolete helpers only after that proof; keep them in the rollback
  bundle if the previous Pier requires them.
- Release evidence must show the distributed artifact set contains no
  standalone capenc, audiocap, input-helper, session-agent, or launcher binary.

### Session admission

A Pier drives **one physical desktop session at a time**. That is a hardware
constraint, not a policy one: a second concurrent session would fight the first
over the same display, input devices, and encoder. The Pier therefore holds a
capacity-one session admission gate, and keeps the slot reserved for a bounded
reconnect window (max two hours) so a dropped Deck can resume rather than lose
the session. Do not remove or widen that gate without understanding what it
protects.

### capenc READY line

capenc emits `[capenc] READY version=1 backend=... codec=... cursor=local|host ...`
on stderr. The pier's `parse_ready_line` requires the `cursor=` field. The
`cursor=` value echoes the `cursor=local|host` argv the pier passes. Old capenc
binaries did not emit this field and will fail with
`capenc READY protocol error: missing cursor`.
