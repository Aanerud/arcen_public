# Windows Host Ownership

**Owner role:** Windows Host

Own Windows capture, input injection, machine authentication integration, host
session lifecycle, and Windows host packaging coordination in this path.

Validate on Windows/MSVC with
`cargo build --locked --release -p arcen-pier-windows
-p arcen-credential-provider`, then run affected crate tests and the root
shared-crate test and strict Clippy gates. There is no single-platform
`--workspace` build. The Credential Provider is logon-path security-critical;
production artifacts require Authenticode signing.

Runtime settings live in `%ProgramData%\Arcen\pier.json`; its common schema is
identical to Linux `/etc/arcen/pier.json`, with Windows-only values under
`platform`.

## Capture and HDR pipeline boundaries

- Auto/Speed are the eight-bit path: probe DDA for real desktop images and
  otherwise use WGC BGRA8. Host cursor authority requires WGC.
- Every ten-bit contract requires a concrete WGC
  `R16G16B16A16Float` scRGB pool. Never fall back to BGRA8 and keep claiming a
  ten-bit stream.
- Grading and HDR are separate conversions over that FP16 source. Grading
  clamps to SDR reference range and re-encodes with the sRGB curve DWM
  linearised with, so the stream keeps the desktop's code values and is
  tagged `srgb` (VUI 13) for a Deck that claims `srgb_transfer`
  (`arcen_media::video::serve_sdr_desktop`, decided in
  `HostConfig::apply_initial_video_request`); an older Deck keeps the
  BT.709 OETF and tag. The GPU shader and the CPU table must apply the same
  transfer (`WideShaderMode` follows `ScrgbSdrTransform::transfer`). HDR converts linear
  scRGB to absolute BT.2020/PQ using 80-nit Windows reference white.
- HDR places the desktop's SDR white at BT.2408's 203 nits, as every Pier
  does: capenc reads the captured output's `DISPLAYCONFIG_SDR_WHITE_LEVEL`
  (Windows composes SDR there; 240 nits by default) and the shared
  `ScrgbPqTransform::with_sdr_white_nits` rescales in linear light, keeping
  highlights in proportion. The helper logs `HDR SDR white: host N nits ...`.
- The broker serves every attachment on isolated QUIC lanes (ADR 0014,
  protocol v5): it grants the attachment before relaying anything the agent
  says, keeps control on the control lane and hands every other agent
  message to the shared `arcen_transport::quic::PierLaneRelay` (video,
  audio, cursor and bulk each on their own lane; a full video backlog drops
  and asks the agent for a keyframe; audio never falls back onto another
  lane). Deck input, microphone and bulk lanes come back to the agent in
  order. Do not reintroduce a single Deck stream.
- Frames in flight: once the agent is streaming, the broker reports every
  video frame the relay released (`ServiceMessage::VideoReleased` from
  `PierLaneRelay::video_released`, a running count per attachment); the
  agent's `OutboundVideoMux` keeps at most one unreleased
  (`arcen_media::video::RelayedVideoWindow`: handed over minus released, never
  reset, one extra frame per quiet second), so a slow path
  holds frames in its per-monitor shared queues under
  `video_backlog_budget`, and the frame router and the capenc reader wait for
  room instead of dropping. Never let frames pile up in the relay again, and
  never block the agent socket for video: audio, cursor and control share it.
  The agent's environment is the signed-in user's (`CreateEnvironmentBlock`
  without inheritance): a service setting the agent must honour, such as
  `ARCEN_VIDEO_BACKLOG=legacy`, crosses in `AgentStart`.
- A Grading, HDR or colour-fidelity refusal leads with the shared
  `FidelityRefusal` code of its cause (`coded_fidelity_refusal`): pass the
  NVENC `UNAVAILABLE` reason, or the reason Auto resolved to OpenH264, to
  `apply_software_h264_backend_after`; never return the bare software
  sentence when the GPU's cause is known.
- Bitrate starts at the shared `link_capped_average_bitrate_bps` (capenc
  `rate_control_sizing`); 22.9 Mbps uncapped HDR made sessions sluggish and
  starved audio on the lab link. The shared controller then moves it live:
  Auto/Speed climb to the shape budget on a clear path and past it, up to
  `PROVEN_PATH_HEADROOM` times, only with per-interval delivery evidence.
- FP16 scRGB conversion uses up to 16 threads (14.6 ms at 1800x1130 on the
  lab EPYC; 8 threads capped HDR at 22.7 fps). GPU conversion is the durable fix.
- The HDR EDID carries the Deck display's own gamut and luminance
  (`ClientMonitor.color` through `arcen_outputs::edid`); the fixed reference
  grade is only for a Deck that reports nothing.
- The HDR EDID declares a 550 MHz HDMI link; a 10-bit mode needs 1.25× its
  pixel clock. A mode past that is offered as SDR only and Windows never
  activates HDR on it (3600x2260@60 stayed `mode=sdr` with HDR switched on
  until the broker killed the agent). Size HDR displays with the shared
  `arcen_outputs::edid::hdr10_refresh_hz` (single display: lower the refresh
  to a multiple of the HDR stream rate; multi-monitor: refuse), never by
  waiting longer.
- Every wait in the agent's setup is bounded by the shared
  `arcen_session::setup_budget` (25 s from the start packet, 5 s short of the
  broker's 30 s readiness wait, via `session::AGENT_SETUP`). A wait that
  reaches it rolls back and refuses with a typed code
  (`HdrSetupRefusal`, documented in `docs/operations/error-codes.md`); never
  let a setup wait outlast the broker, which terminates the agent and leaves
  the crash watchdog to restore the display.
- HDR additionally requires the final HDR EDID/topology, Windows 11
  `activeColorMode=HDR`, and DXGI
  `RGB_FULL_G2084_NONE_P2020` on the exact session-bound display target.
  Never count or mutate unrelated active outputs.
- Keep pre-provision NVIDIA EDID recovery armed for the complete display lease
  and restore it after normal attachment teardown. A remote request must not
  leave persistent display identity or HDR state behind.
- On disconnect, atomically close and discard outbound video queues before
  joining the writer so buffered frames cannot consume the reconnect window.
- Local cursor: `cursor_watcher.rs` reads the desktop's cursor bitmaps only
  for a Deck that declared `cursor_image_v1`, hands them to the shared
  `arcen_input::cursor::HostCursor` and sends `CursorUpdate` records with the
  image always inline; the broker's lane relay owns the lane cache
  (`arcen_protocol::cursor::CursorRecordRelay`). Never send an agent record
  without its image, and never strip images in the agent.

Escalate shared API or protocol changes to Shared/Architecture; authentication,
privilege, GPU, signing, packaging, and release changes to Release/Security.

## Reaching a Windows Pier

Install prefix: `C:\Program Files\Arcen\Pier\arcen-pier.exe`.
Service: `ArcenPier`. Runtime config: `%ProgramData%\Arcen\pier.json`.

Do not record real hostnames, IP addresses, or credentials in this repository —
it is public. Note that on a domain-joined host the local administrative account
is often **not** `Administrator`; guessing through account names risks a
lockout, so confirm the account out of band.

```powershell
Get-Service ArcenPier
Restart-Service ArcenPier -Force
Get-NetUDPEndpoint -LocalPort 18444 | Format-List LocalAddress,LocalPort,OwningProcess
qwinsta            # who owns the console, and in what state
```

### Read `qwinsta` before blaming the Pier

A refusal reading `Remote sign-in is unavailable: another account is actively
using the physical console.` is
[`docs/architecture/windows-console-policy.md`](../../docs/architecture/windows-console-policy.md)
behaving correctly, not a fault: another account holds the console, and **a
locked console still counts as Active**. Restarting `ArcenPier` will not change
it. Either connect as the account `qwinsta` shows owning the console, or sign
that account out.

### The installer and the documented manual path must stay in step

`hosts/windows/INSTALL.md` hardens ACLs with SIDs (`icacls /setowner
'*S-1-5-32-544'`, SID-based `/grant:r`) and has done so from the start, so the
documented manual install has always worked on a localized Windows.

The `install-arcen-pier.exe` written later granted to `Administrators` and
`Users` **by name** in its very first commit, and never
worked on a non-English Windows at all. It was not a regression — the two paths
simply diverged, and every machine that exercised the binary (including CI's
`windows-latest`) is English, so nothing caught it for four
weeks.

When changing one path's privilege or ACL handling, check the other.

### No Wintab and no Native tablet on this host (ADR 0017)
Pens reach Windows through Windows Ink only: `PenInjector` in `src/input.rs`
(`CreateSyntheticPointerDevice` / `InjectSyntheticPointerInput`). Do not add a
`wintab32.dll`, DLL injection, or an emulated tablet maker's device.

Native tablet (`wacom_usb_bridge` on the wire, a legacy name for the
vendor-neutral mode) is not offered: it needs a signed kernel driver (UDE/VHF;
EV certificate, attestation, WHQL/HLK) the project does not fund. The Pier
advertises `usb_tablet_bridge: Unavailable` and refuses a request with a
reason (`resolve_windows_tablet_mode_result` in `src/session.rs`). Keep Windows
connections on Tablet support. See
[ADR 0017](../../docs/adr/0017-forward-only-pen-input.md); ADR 0012 records the
route that is not taken.
