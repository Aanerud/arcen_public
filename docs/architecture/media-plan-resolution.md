# Media Plan Resolution and Portable Software H.264

**Status: implemented and physically accepted for the active direct-connection
Linux and Windows Piers (updated 2026-09-01).** Protocol v4 carries auth-time
video intent so both Piers resolve the final capture provider, conversion,
encoder, cursor authority, and colour contract before `ServerHello`.

This document is scoped to video capture/encode plan resolution. Audio,
microphone, and observability have their own contracts and do not select or
merge these video pipelines.

## Shared-first boundary

`arcen-media` owns the platform-free frame, conversion, media-plan, and optional
software-encoder contracts:

```text
Auto / Speed
  Windows: DDA(real-frame proof) or WGC BGRA8 -> explicit YUV conversion -> NVENC
  Linux:   NvFBC -> CUDA -> NVENC                         [device-to-device]

Grading
  Windows: WGC FP16 scRGB -> SDR OETF/BT.709 -> I444 P16 -> NVENC
  Linux:   depth-30 Xorg -> XShm RGB10 -> I444 P16 -> CUDA upload -> NVENC

HDR
  Windows: HDR EDID/topology/exact-target proof -> WGC FP16 scRGB
           -> BT.2020/PQ -> I444 P16 -> NVENC
  Linux:   Xorg cannot prove HDR -> resolve to the Grading pipeline + degradation

Shipped software floor
  Windows: WGC BGRA8 -> checked I420 -> source-built OpenH264 -> Annex-B
  Linux:   X11 BGRA -> checked I420 -> source-built OpenH264 -> Annex-B

Comparison-only Windows path
  WGC BGRA8 -> checked NV12 -> inbox Media Foundation H.264
```

The capture provider is part of resolution truth, not an implementation detail.
The eight-bit fast path is never widened to implement Grading/HDR, and a
ten-bit container never upgrades an eight-bit source by declaration.

For multi-monitor, each codec is an aggregate candidate spanning the complete
monitor roster. Admission measures uniform AV1 first, then uniform HEVC, then
uniform H.264 on the approved adapter set; only afterward may an
operator-enabled mixed hardware/software candidate be considered. Per-monitor
codec fallback is forbidden.

The implementation is split across:

- `shared/media/src/video/frame.rs`: checked borrowed/mutable NV12 and I420
  plane views with explicit dimensions, strides, overflow checks, and exact
  active prefixes;
- `shared/media/src/video/convert.rs`: allocation-free BT.709 limited-range
  BGRA-to-NV12 and direct BGRA-to-I420 full/even-row-range conversion;
- `shared/media/src/video/plan.rs`: `EncoderRequest`, concrete
  `EncoderBackend`, limits, typed unavailability, `ResolvedMediaPlan`, and
  strict READY/UNAVAILABLE v1 formatting and parsing;
- `shared/media/src/video/intent.rs`: bounded auth-time intent/token/capability
  validation and the client-decodable AV1 → HEVC → H.264 preference;
- `shared/media/src/video/obu.rs`: shared AV1 low-overhead OBU recovery-frame
  classification used by both Piers;
- `shared/media/src/video/software_h264.rs`: the sole Arcen OpenH264 caller,
  enabled only by `software-h264-source`; and
- `shared/media/src/video/mod.rs`: the public video surface.

`arcen-media` remains `#![forbid(unsafe_code)]`, platform-free, and free of
hidden I/O. The safe wrapper does not make the underlying C/C++ codec
memory-safe. Platform FFI and native resource ownership stay in `arcen-capenc`.

## Resolved-plan and failure contract

The Deck places its requested colour axes, FPS ceiling, codec-selection intent,
and measured decode capabilities in `AuthResponse.initial_video`, alongside the
display request. The concrete plan is established before `server_hello`.
Capenc argv, codec,
chroma, dimensions, fps, cursor authority, capabilities, every video header,
and the later `quality_settings` consistency echo are derived from that plan.
READY v2 must exactly
match the request and attempt correlation ID. Unknown, missing, duplicate, or
contradictory fields fail closed.

Auto fallback advances only after a typed concrete-backend UNAVAILABLE response.
Malformed READY, unsupported geometry, initialization errors, corrupt output,
and mid-session failure do not advance or hot-switch the codec. Reconnect
performs a new resolution.

Protocol-v3 `display_update` is preserved. On Linux, a resize creates a new
generation of the already-resolved stream rather than a new fallback attempt:
the host pauses and clears the bounded video queue, waits for the writer to
finish any old-size access unit, stops capture, drops the old child's borrowed
and mapped capture state, and then retargets the display. The replacement child
is pinned to the current concrete backend and must return READY with the exact
applied geometry and otherwise identical codec, chroma, fps, cursor, and
capability truth. The host recreates absolute input geometry and obtains a
recovery IDR before writing the accepted `display_update_result`; the queued
IDR is released only after that control write. No old-size access unit can
follow the result, and no backend or format fallback is allowed during resize.
Failure after retiring the old generation is terminal.

The held display records the applied size, so a reconnected attachment resolves
a fresh plan against that size before its new hello and IDR. A reconnect may
select a different backend only before hello and only if the held display
satisfies its limits; it never introduces a hidden second modeset. Windows
continues to advertise `supports_display_update=false`; every Windows
attachment still resolves a fresh READY-backed pipeline before hello.

The portable contract is H.264 Baseline, YUV420, BT.709 limited range,
screen-content real-time usage, bitrate rate control, no B-frame path, at most
1920x1080 at 30 fps, a two-second intra period, and recovery IDRs containing SPS
and PPS. Output is Annex-B and bounded to 16 MiB per access unit. The wrapper
retains its output and parameter-set storage and counts capacity growth; native
OpenH264 allocations remain opaque.

## Windows adapter

For adaptive Performance, Windows `auto` tries AV1, HEVC, and H.264 on the
selected NVENC adapter before source-built OpenH264.
Colour-fidelity and exact requests do not silently enter the ordinary codec
ranking; backend unavailability may still reach the truthful OpenH264 floor.
The host predicts the display policy from the selected adapter, but preserves
`auto` in the capenc request so a compatible NVENC initialization failure can
still advance to OpenH264. Only the resulting canonical READY becomes
the backend advertised in hello and frame headers.

Capture selection is independently constrained by the resolved colour
contract. Eight-bit requests may probe DDA and fall back to WGC BGRA8. Every
ten-bit request requires WGC FP16 scRGB. Grading applies an SDR transform and
does not enable HDR. HDR first provisions the final sink/topology, enables and
verifies HDR on the exact bound target, and then applies the absolute
BT.2020/PQ transform. FP16 refusal fails closed.

MF display negotiation carries the macroblock invariant into fixed-mode
fallback selection. The requested size is aligned first; if the driver rejects
that custom mode, only supported modes whose width and height are both divisible
by 16 enter fitting, ranking, and truncation. An unaligned current mode cannot
serve as the terminal fallback. This policy is MF-specific and does not change
NVENC `ExactIsolated` or the ordinary negotiated MF path.

`hosts/capenc/src/win_mf.rs` supplies WGC/staging capture for MF, which consumes
the shared BGRA-to-NV12 implementation. Existing WGC cursor, damage hysteresis,
refresh, and framing ownership remain in capenc. Windows Pier builds hello and
frame truth from the returned plan.

The Windows release build is MSVC-only and sets Rust `+crt-static` plus `/MT`
for native C/C++ compilation. Packaging inspection rejects dynamic MSVC CRT,
GNU C++/thread runtimes, OpenH264 DLLs, and nested codec payloads from native
archives and packaged binaries. These checks do not replace Authenticode; the
current release automation/signing boundary remains deferred.

## Linux adapter

Linux `auto` first performs the bounded native NVENC probe. An AV1 request
retains the HEVC retry on older generations; adaptive Performance additionally
tries hardware H.264 before typed unavailability selects OpenH264. Explicit
software mode skips the NVIDIA path. Software limits are selected before the
one display mutation.

Capture selection is keyed on resolved bit depth. Eight-bit requests use
NvFBC/CUDA and retain the existing device-to-device path. Every deeper request
uses depth-30 Xorg/MIT-SHM and a host RGB10 conversion plus one CUDA upload.
Because Xorg supplies no HDR composition/metadata contract, PQ/HLG requests are
rewritten to the Grading BT.709 contract before capenc starts. XShm cannot
composite a host cursor, so Host authority degrades to Local only on the wide
path.

`hosts/capenc/src/linux_x11.rs` owns authenticated X11 connection, XRandR output
selection, XDamage activity, MIT-SHM 1.2 mapping lifetime, bounded XGetImage
degradation, layout validation, modeset recreation, and the
BGRA-to-I420-to-OpenH264 loop. The first READY is emitted only after capture,
conversion, and an encoded access unit succeed. The implementation captures a
full image on a damaged tick and uses shared `IdleCadence`; selective retained
conversion is not enabled. Linux packaging compiles the engine into the Pier's
`capenc` subcommand, installs no OpenH264 shared object, and retains the legal
notice file.

## Governed dependency and build model

The only approved codec strategy is:

```toml
openh264 = { version = "=0.9.7", default-features = false }
openh264-sys2 = { version = "=0.9.7", default-features = false }
software-h264-source = [
    "dep:openh264",
    "dep:openh264-sys2",
    "openh264/source",
    "openh264-sys2/source",
]
```

Cargo resolves `openh264` **0.9.7** and `openh264-sys2` **0.9.7** from
crates.io. The latter compiles the Cisco source tree it supplies at upstream
commit `a8e04adb69c79757da014007d4694684a64c7b74` into static archives. Arcen does
not enable `libloading`, download a Cisco runtime, include Cisco's precompiled
binary, or package an OpenH264 DLL, dylib, framework, or nested codec archive.
Changing this source-only model requires a new Release/Security and legal
decision.

The default `arcen-media` graph contains none of `openh264`,
`openh264-sys2`, `wide`, `nasm-rs`, `cc`, or `walkdir`. The source feature
requires:

- current stable Rust satisfying **Rust 1.89 or newer**: the wrapper crates
  declare 1.85, but resolved `wide` 1.1.1 and `safe_arch` 1.1.0 declare 1.89;
  the dependency-light workspace default retains its documented 1.85 floor;
- a target C++ compiler (`cl.exe` under x64 MSVC, or the hosted platform's
  C++ compiler); and
- NASM on x86/x86_64. Upstream can silently omit assembly if NASM fails, so
  Arcen release/CI entry points preflight NASM rather than treating it as an
  optional performance accident.

Exact crate checksums, the Cisco source commit, license notices, and the
feature-specific SBOM inventory are recorded in
[`../../legal/ORIGINS.md`](../../legal/ORIGINS.md) and
[`../../legal/THIRD_PARTY_NOTICES.md`](../../legal/THIRD_PARTY_NOTICES.md).
The release SBOM must still be regenerated for the complete target-specific
lockfile graph; current hosted CI does not claim SBOM generation or signing.

## Copyright, patent, and distribution boundary

The Rust wrappers and Cisco source declare BSD-2-Clause, and binary
redistribution must reproduce the applicable copyright, conditions, and
disclaimer. BSD-2-Clause review is not a patent clearance.

H.264/AVC implementations and distribution can implicate patent rights,
territory-specific obligations, product use, and pool or bilateral licensing.
The Cisco prebuilt-binary program is not used, and Arcen makes no claim that its
binary-module terms or Cisco-paid royalties cover Arcen's source-built
artifacts. Before any external distribution, Release/Security and legal must
approve the actual territories, use, licensing posture, notices, SBOM, and
signed/notarized artifacts. This document records engineering facts, not a
legal conclusion.

## Automated and physical acceptance

Hosted CI can check:

- default shared tests/strict Clippy and default graph exclusion;
- source-feature `arcen-media` build, tests, and strict Clippy on Linux,
  Windows/MSVC, and macOS with compiler/NASM preflight;
- platform-appropriate capenc feature matrices;
- strict READY/UNAVAILABLE, plan policy, frame geometry, conversion goldens,
  Annex-B/SPS/PPS/IDR, output-cap, and buffer-reuse behavior; and
- package dependency/payload assertions.

Hosted CI cannot claim physical capture, GPU fallback, signing, notarization, a
release SBOM, or protected hardware results. Release evidence must include:

- Windows VMware MF behavior and unchanged NVENC/MF auto behavior;
- Linux dedicated-Xorg explicit software mode without NVIDIA libraries, GRID
  NVENC selection, and non-NVIDIA typed fallback;
- macOS Deck decode, IDR recovery, reconnect, display restore, and idle cadence;
- weakest-supported-host sustained 1080p30, p95
  capture+convert+encode below 70% of the 33.3 ms budget, no frame gap over
  150 ms, output-capacity/allocation-growth accounting after warm-up, CPU,
  bitrate, and bounded memory; and
- at least a one-hour physical soak plus modeset, failure, teardown, and restore.

### Recorded partial Windows VMware evidence

On 2026-07-24 an isolated Windows 11 VMware guest with PCI vendor `0x15ad`
and the inbox `Microsoft Basic Render Driver` completed a bounded explicit-MF
probe. Interactive `diagnose-host` resolved one attached console output, D3D11
feature level 11.0, no NVENC runtime, and the inbox software H.264 MFT. Capenc
then opened that output through WGC, initialized MF H.264 Main at
1024×768/30 fps, produced its first in-memory access unit, and emitted canonical
READY v1 with `backend=media-foundation-sw-h264`, H.264/YUV420, and truthful
capabilities.

This proves only console capture, conversion, explicit MF initialization, first
access-unit production, and READY truth on that guest. It does not claim
unchanged NVIDIA runtime behavior, Deck end-to-end acceptance, sustained
performance, reconnect/restore, one-hour soak, signing, or distribution
approval; those listed gates remain open.

A later Proxmox CPU-only acceptance pass used SPICE/QXL-compatible display
hardware and SPICE-backed ICH9 HDA. Windows exposed the display through
`Microsoft Basic Render Driver` with D3D11 feature level 11.0, no D3D11 video
device, and the inbox MF software H.264 MFT. The fixed-mode driver rejected
1792×1168 and the MF-specific aligned fallback selected 1280×800. Deck received
the truthful MF/H.264/YUV420 plan and the session was usable at 30 fps.

Before the emulated audio device existed, WASAPI had no default console render
endpoint and retried without affecting video. With ICH9 HDA present, loopback
opened the default 48 kHz stereo endpoint; the observed successful session sent
1,826 audio packets with zero host queue drops, capture errors, or restarts.
Deck recorded no playback underruns. These observations establish functional
CPU-only media behavior, not sustained-performance, resize, soak, signing, or
distribution acceptance.
