# Changelog

All notable changes to Arcen are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Arcen uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.13.0] — 2026-09-27

The first release with a macOS Pier package, Linux HDR, and hosts that follow
the Deck's displays. Every streaming preset was measured end to end on the lab
hosts with the Deck GUI.

### macOS Pier (preview)

- A working host. A `_arcen` network service (LaunchDaemon: UDP 18444, TLS,
  admission, relay) and a per-session desktop agent (capture, encode, input,
  clipboard, audio) share the `arcen_session::agent_relay` contract.
- The login window streams, and the Deck follows sign-in into the desktop.
  Input at the login window uses the entitled virtual HID keyboard and
  absolute pointer (`com.apple.developer.hid.virtual.device`).
- Speed, Grading and HDR are separate pipelines, proven in the bitstream.
  Grading is RExt 4:4:4 10-bit BT.709; HDR is RExt 4:4:4 10-bit PQ/BT.2020,
  claimed only on proven EDR headroom, with SDR white at 203 nits.
- A signed and notarised installer package with sysadmin-style install and
  `uninstall.sh --purge`. Privacy permissions must be approved at the Mac, so
  the command-line `installer` is refused by default.
- Still preview: no cold-boot LoginWindow path, and multi-monitor is not
  hardware-qualified.

### Deck

- Each screen's colour facts (P3 gamut support and EDR headroom) are sent to the
  host, which builds its virtual display from them. HDR is greyed out ("No
  HDR-capable screen found") when no screen can show it.
- Command shortcuts reach a macOS host (the Cmd→Ctrl swap is off for Aqua).
  Holding ⌘Q for 3 s quits the Deck, as in Chrome; a tap goes to the host.
- Logs the received stream truth (SPS), a per-plane distinct-code census, and
  a "stream delay" record with video and audio age at p50 and p95.

### Linux Pier

- Added `video.desktop_encoding`. `rec2100-pq` declares that a colour-managed
  application (Flame's HDR UI) writes Rec.2100 PQ into the depth-30 Xorg
  desktop, so an HDR request is passed through as PQ / BT.2020 instead of
  resolving to Grading. The default `sdr` keeps the previous behaviour.
- A session's virtual head is the Deck's display, by per-session EDID.
- Audio travels on its own priority QUIC stream, and the frame pump applies
  backpressure instead of requesting keyframe storms.

### Windows Pier

- HDR EDIDs carry the Deck display's gamut and luminance, and HDR places SDR
  white at 203 nits, as the macOS Pier does.
- Audio travels on the shared priority stream; the FP16 scRGB conversion uses
  up to 16 workers, bringing HDR to its target frame rate.
- `video.desktop_encoding` is rejected unless `sdr`: Windows reads HDR state
  from the operating system.

### Shared

- `DisplayColorMsg` and `arcen_media::display_color`; EDIDs carry gamut
  chromaticity and HDR luminance.
- `link_capped_average_bitrate_bps` is the one bitrate rule for every Pier.
- `PriorityAudio` QUIC stream in `arcen-transport`.
- `hevc_sps` stream truth, `pq_white` 203-nit contract,
  `DesktopSignalEncoding` / `constrain_to_desktop_encoding`.
- A host that has not measured its encoders follows the client's codec.

### Fixed

- The Linux and Windows Piers build again (missing hello fields).
- The shared codec resolver no longer turns Grading into H.264.
- The Linux and Windows capture encoders no longer overshoot the bitrate cap.

## [0.12.0] — 2026-09-12

### macOS Pier (new, in progress)

A native macOS host now exists as `hosts/macos`. It authenticates through PAM,
serves QUIC on UDP 18444 with its own certificate lifecycle, captures and
encodes the desktop through ScreenCaptureKit and VideoToolbox, injects pointer
and keyboard input, carries typed pen events, sends host audio when consent
exists, and carries the clipboard. It is not yet a complete Pier: there is no
published installer, no cold-boot LoginWindow path, no native USB tablet, and
multi-monitor is implemented but not hardware-qualified.

- **The real Deck now completes an authenticated session against it.** Three
  defects each ended every real session and none was caught by the tests,
  because host and test were both hand-written JSON and so agreed with each
  other rather than with the client: `server_hello` was sent before
  `auth_request`, which selects the Deck's no-authentication path and zeroizes
  the password; the password was read from a `password` field when the Deck
  sends it in `credential`; and `server_hello` omitted `negotiated_transport`,
  which the Deck requires, while misspelling `supports_h265`. All three are
  fixed by building the shared message types instead of hand-writing JSON.
- Clipboard now uses the real framed protocol. The host had invented a
  text-offer-then-bare-bytes scheme that no Deck speaks; it now sends and
  receives `FrameType::Clipboard` chunks through the shared
  `ClipboardReassembler`, and dispatches binary messages by frame type instead
  of treating every one as a clipboard payload.
- Host audio capture with a mandatory local-mute lease, built on public Core
  Audio process taps. Real audio is captured while the host's own speakers stay
  silent, which is the point of the feature.
- Host audio negotiation no longer deadlocks on itself. The host advertises
  from the Screen & System Audio Recording grant, publishes `audio_output`,
  sends `audio_stream_result`, and starts capture on a separate thread under a
  1500 ms budget. If consent is missing or Apple's prompt blocks inside tap
  creation, the Deck is told `CaptureUnavailable` and video continues.
- The macOS multi-monitor wire path is present: auth-time offer/request,
  applied topology in `server_hello`, and region video headers carrying monitor
  id, topology generation, and stream epoch. It is not claimed as proven because
  the lab Mac used for the latest run had one display attached.
- `packaging/macos/build-pier-app.sh` now builds the release Pier binary before
  bundling it, instead of packaging an arbitrary stale file from
  `target/release`.

### Shared

- `arcen-media` gained `clipboard::policy_message`, moved out of the Linux host
  before a second copy was written for macOS, and `audio::PcmPacketizer`, which
  frames arbitrary capture buffer sizes into fixed audio-v1 packets.
- `arcen-session` gained `audio.local_playback`, defaulting to `muted`. It is
  deliberately separate from `audio.enabled`: someone beside the host machine
  hearing the remote user is a privacy failure whether or not audio is being
  transmitted. Existing configurations parse unchanged and acquire the safe
  default.

### Fixed

- **Windows certificate renewal replaced the key it promised to preserve.**
  The migrated provisioning path printed "reissuing the TLS certificate" for
  renewal and adoption, then generated a new key anyway. Automatic renewal near
  expiry would have changed the host identity and broken every SPKI-pinned
  Deck, without the warning a deliberate rekey prints.
- **Linux `new-host-cert` produced certificates with no subject alternative
  name.** It parsed `--dns` and `--ip` and discarded them, so every Deck
  rejected the result during the TLS handshake while the material looked
  correct on disk. The shell helper in `packaging/linux/` has always done this
  properly.
- macOS: a rejected QUIC handshake ended the listener, so one wrong-ALPN
  connection could take a host offline; closing a stream could deadlock the
  serving loop; two seconds of desktop idleness ended a session on a
  damage-driven capture API; the Core Audio callback allocated on the
  real-time thread; failed audio teardown was recorded as success, which can
  leave a Mac silent; any authenticated account was served the console user's
  desktop; clipboard queues were unbounded; and `serve --once` reported
  success after a stream failure.
- `PcmPacketizer` accepted a frame specification rounding to zero samples,
  where `push` emits empty packets forever without consuming input.

### Validated

Linux (Rocky 9.5, 731 tests) and Windows 11 (688 tests) were built and tested
on their own operating systems after every shared change, rather than
cross-checked from macOS. The real Deck completes an authenticated session
against the macOS Pier.


## [0.10.0] — 2026-09-01

### Streaming presets and pipeline separation

- Replaced independent performance/colour controls with four complete product
  presets: **Auto**, **Speed**, **Grading**, and **HDR**.
- Kept the fast 8-bit and fidelity pipelines separate. Auto/Speed do not pay
  the host-copy or conversion cost required by Grading/HDR.
- Extended negotiated session truth with primaries and transfer degradation so
  the Deck can distinguish ten-bit SDR from real HDR and show every fallback.

### Windows

- Added a genuine ten-bit Grading source: WGC
  `R16G16B16A16Float` scRGB converted to full-range BT.709 I444 P16 before
  NVENC. FP16 refusal fails closed instead of repacking BGRA8 as ten-bit.
- Added end-to-end HDR: session-scoped NVIDIA HDR EDIDs, exact topology,
  Windows 11 distinct HDR state, DXGI PQ/BT.2020 verification, WGC FP16
  capture, linear-primary conversion, 80-nit-reference absolute ST 2084, and
  HEVC 4:4:4 10-bit output.
- Scoped HDR state changes to the exact session display targets and kept the
  pre-provision EDID recovery journal armed through lease teardown.
- Fixed forced-loss resume by atomically discarding buffered video before the
  writer is joined; credential-free reconnect now retains the desktop and
  returns to healthy media.
- Fixed Windows installer public ACL convergence on localized or previously
  installed systems.

### Linux

- Preserved NvFBC → CUDA → NVENC as the device-to-device 8-bit path.
- Added a separate depth-30 Xorg/MIT-SHM pipeline for genuine RGB10 capture,
  mask-derived `XBGR2101010` handling, shared P16 conversion, and one CUDA
  upload before NVENC.
- Made Xorg HDR requests resolve truthfully to Grading BT.709 SDR. Real Linux
  HDR remains gated on a future color-managed Wayland provider.
- Degraded host cursor authority to the Deck-local cursor only for the XShm
  wide path; NvFBC host cursor behavior is unchanged.

### macOS Deck

- Added native VideoToolbox `xf44` retention and a dedicated
  `RGB10A2Unorm` Metal presentation layer for Grading and HDR.
- Enabled ITU-R BT.2100 PQ, HDR10 metadata, and EDR only when the resolved host
  transfer is PQ. Normalized PQ output uses Apple's 10,000-nit optical scale.
- Preserved negotiated BT.709, sRGB, PQ, and HLG transfer metadata through
  VideoToolbox and made every 8-bit presentation fallback permanently visible.

### Release validation

- Completed Windows and Linux Auto/Speed/Grading/HDR matrices with decoded
  frames, nonzero audio, keyboard/pointer input, cursor authority, display
  restore, and forced-loss credential-free resume.
- Rebuilt the Linux and Windows single-file installers and the Developer ID
  signed, notarized, stapled macOS Deck.

## [0.9.8] — 2026-08-25

**The first public release.** Arcen was developed privately and is published
here for the first time, as free software under the AGPL-3.0. There is no
earlier public version; the history before this point was private and is not
part of the public record.

This release is numbered 0.9.8 rather than 1.0.0 deliberately. All three product
crates build and pass their tests on their target OS, and the Linux Pier and
macOS Deck have carried real sessions — but interfaces may still move, the
gateway does not exist, and only two of the six host/client combinations are
implemented. 1.0 should mean more than "it compiles".

### Transport and trust

- **QUIC-only product transport** on UDP 18444, with TLS 1.3 at the transport
  layer. There is no TCP fallback in shipped binaries.
- **Certificate trust model** with five explicit modes: system CA, private CA
  bundle, trust-on-first-use pending, TOFU-pinned, and a development-only
  insecure mode that is **double-gated** and refuses to engage unless both the
  configuration mode and an explicit CLI flag agree.
- **TOFU pairing ceremony** — on first connection the Deck shows the
  certificate SHA-256, the SubjectPublicKeyInfo SHA-256, and the validity
  window, and offers cancel / trust once / trust and remember.
- **Certificate pinning** on both whole-certificate and SPKI digests, compared
  in constant time, persisted per saved connection with the pin kind, the time
  it was pinned, and an optional label.
- Host certificate generation at install time covering the machine's real names
  and addresses.
- **Session auto-reconnect** with a bounded reconnect window that holds the
  session slot rather than tearing the desktop down.

### Video

- **NVENC hardware encoding** for H.264 and HEVC, including **HEVC 4:4:4 10-bit
  full-range** for grading work, and 4:2:2 support on Blackwell-generation
  hardware.
- **AV1 encoding** via `rav1e` for a royalty-free path.
- **Software H.264** fallback via OpenH264, behind an opt-in feature so the
  default dependency graph stays free of the native build chain.
- **Colour fidelity** work covering 10-bit and 4:4:4 pipelines end to end.
- **Multi-monitor** capture and presentation with a shared output-provider
  lifecycle.
- **Retina / effective stream resolution** handling on the macOS Deck.
- Region-based screen update patching.

### Audio

- Opus audio compression for host-to-client audio.
- Microphone / audio input redirection from the Deck to the host session.

### Input and devices

- Keyboard, mouse, scroll, and region input.
- **Pen tablet input** with pressure support.
- **Hard USB (USB-over-IP) device passthrough**, one tablet per seat, with a
  privileged macOS helper on the client side. Linux hosts only.
- **Timezone redirection** so the remote session reflects the client's timezone.

### Hosts

- **Linux Pier** with a dedicated Xorg session model, PAM authentication, and a
  single fused multicall binary that embeds the capture, audio, input-helper,
  session-agent, and session-launcher subcommands rather than shipping separate
  executables.
- **Windows Pier** with an IDDCX virtual display driver and a Windows
  Credential Provider participating in the logon path, plus a console-ownership
  policy that refuses a remote sign-in when a local account holds the physical
  console.
- Self-contained installers for both platforms that lay out directories,
  generate the host certificate, register and start the service, and open the
  firewall port.

### Clients

- **macOS Deck**, a native client with its own decode and render path, saved
  connection bookmarks, and per-connection trust configuration.

### Observability

- A dedicated OS-free tracing and bounded-I/O runtime (`arcen-observability`)
  and a pure event contract crate (`arcen-telemetry`), with a conformance
  validator run in CI.

### Removed before publication

- **The entire commercial licensing system.** Arcen previously carried an
  offline, signed, node-locked licensing stack — roughly 12,000 lines across a
  shared crate, both host adapters, and an issuer tool. Under the AGPL it serves
  no purpose, and it was the only thing preventing the software from running.
  The single-session admission gate it contained was kept and re-homed, because
  that constraint is physical: a Pier drives one desktop.
  Removing it also dropped an entire cryptographic dependency subtree from the
  build — `ed25519-dalek`, `curve25519-dalek`, `ed25519`, `signature`, `pkcs8`,
  `der`, `spki`, `const-oid`, `base64ct`, and `fiat-crypto`.
- **Roadmap components not yet real**: the gateway, the Windows Deck, and the
  shared test-kit crate were removed from the published tree rather than shipped
  as dead code.
- **`arcen-session`'s opt-in `authoritative-session` state machine**, which
  depended on the licensing crate, went with it. The dependency-light
  restore-lease, deskside and direct-reconnect surfaces — the parts actually in
  use — are unaffected.

### Changed before publication

- **AGPL section 13 source offer is built into the programs.** Arcen is
  remote-access software, so people routinely interact with a Pier *over a
  network* rather than running it themselves. Both Piers now carry the licence
  and the source location in their startup banner (and the Linux Pier in
  `--version`), and the Deck carries it in its startup banner. A user who only
  ever receives a built binary can still find the corresponding source.
- **NVENC bindings are now clean-room.** The checked-in NVENC FFI bindings were
  bindgen output derived from NVIDIA's Video Codec SDK header, which cannot be
  redistributed. They are regenerated from
  [nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers) `n12.1.14.1`,
  the MIT-licensed clean-room header set, vendored under
  `third_party/nv-codec-headers/`.

  The vendored header is deliberately **API 12.1 — the same version the previous
  bindings targeted** — so this is a purely legal change with no behavioural
  difference and no increase in the minimum NVIDIA driver (530.41.03 on Linux).
  A first attempt vendored the newest tag, `n13.1.15.0`; it compiled cleanly,
  passed every unit test, and then failed on real hardware with
  `NV_ENC_ERR_INVALID_VERSION`, because API 13.1 requires driver 610.0+ and the
  test host ran 570.172.08. Verified on a live GRID V100D: capture and encode
  initialise and run.

### Verified on real hardware

Everything below was built and tested on its target OS before release:

- **Linux Pier** — builds release; 710 Pier tests and 163 capenc tests pass.
  NVENC capture and encode verified live on a GRID V100D (driver 570.172.08):
  CUDA init, NvFBC capture, and `NVENC ready: 2560x1600 codec=h264`.
- **Windows Pier** — builds release on MSVC; 676 Pier tests, 192 capenc tests
  and 51 credential-provider IPC tests pass.
- **macOS Deck** — builds; 926 tests pass.
- **Shared crates** — 694 tests, strict Clippy clean.

### Known limitations

- Arcen is designed for **direct machine-to-machine connections on a trusted
  network**. It is not hardened for direct exposure to the public internet.
- Released binaries are not code-signed on every platform; expect Gatekeeper and
  SmartScreen prompts.
- CI runs a Linux-only, manually triggered gate. Platform builds are not
  verified automatically.
- macOS Pier, Linux Deck, and Windows Deck do not exist.

[0.10.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.10.0
[0.9.8]: https://github.com/Aanerud/arcen_public/releases/tag/v0.9.8
