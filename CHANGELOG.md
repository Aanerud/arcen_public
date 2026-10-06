# Changelog

All notable changes to Arcen are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Arcen uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.15.0] — 2026-10-06

### Streaming

- macOS Pier advertises host audio again for signed-in sessions. Asking for
  helper permissions in turn (28 Sep) replaced the only call that marked audio
  available, so every hello said `audio: false` and the Deck never requested
  sound; it is now set on every agent and serve path before any prompt.
- Pier debug logging now makes Keel QP-map effectiveness provable live:
  Windows and Linux capenc inherit the Pier debug profile for helper
  diagnostics, and Linux XShm/NVENC stats report per-second biased/neutral
  QP-map counts plus mean dirty fraction alongside the capture damage source.
- macOS Deck native 8-bit and 10-bit video layers now use a shared
  refresh-sequence pacer driven by the window's `CVDisplayLink`: FIFO depth is
  capped at three, overflow drops the oldest frame, stale frames are discarded
  after coarse age caps, each submitted drawable uses a half-refresh minimum
  display duration, and telemetry separates SUBMITTED cadence from
  nonzero-`presentedTime` confirmations while reporting overflow/stale/hidden
  drops, source-cadence underruns, queue depth, refresh gaps and ESTIMATED
  receive-to-callback latency.
- macOS Deck no longer discards a paced frame when every drawable slot is
  still in flight: the refresh waits (`slot_waits` telemetry) and the frame is
  shown on the next one. A full one-refresh minimum display duration made
  presents miss their vsync and cost about one frame in four on a 60 Hz panel.
- The Deck video inbox now holds ~500 ms (30 packets) for every pipeline, so a
  Wi-Fi/VPN stall's burst is decoded instead of cleared; the old 8-packet inbox
  turned 250-300 ms stalls into a repeating keyframe loop.
- Windows Pier QP maps now use OS damage on the NVENC GPU path: Desktop
  Duplication dirty/move rects and WGC dirty regions feed Keel without CPU
  readback, and logs truthfully report `damage_source` plus disengage when no
  damage source exists.
- Windows Pier multi-monitor topology now programs rotated CCD outputs with
  oriented source surfaces and native target timings, so mixed
  landscape/portrait Deck layouts keep truthful READY geometry and no longer
  ask Windows to apply an impossible portrait target mode.
- Streaming pipelines now own a shared keyframe policy: hardware encoders stop
  inserting two-second periodic IDRs and rely on Deck-requested recovery, while
  software fallback keeps a longer bounded safety refresh.
- macOS Pier now honours `redirection.timezone` without adding a second helper:
  the launchd-managed Agent Helper validates the Deck IANA time zone against
  zoneinfo and sets `TZ` in its own GUI session, so newly launched apps inherit
  it. The state lives in that session (an owner-tagged `ARCEN_SESSION_TZ`
  sentinel), is restored on session end and SIGTERM, and dies with logout. A
  user-defined `TZ` is left alone. Upgrades remove the earlier root timezone
  helper and restore any system time zone it changed.
- Pipeline queue contracts now distinguish raw capture overflow from encoded
  access-unit loss: raw frame pressure is latest-wins and does not request an
  IDR, while encoded packet loss keeps the shared rate-limited keyframe
  recovery path. Windows, Linux and macOS hosts report the relevant drop
  counters without noisy per-frame diagnostics unless debug logging is enabled.
- Deck keyframe requests that arrive inside a Pier's short request guard are
  now coalesced and delivered when the guard expires instead of being dropped
  (Linux and macOS Piers; Windows already forwarded every request), and a
  recovery handoff that fails is retried on its own deadline.
- The Deck now normalizes host colour metadata through shared media before
  deciding presentation retags, so equivalent default and explicit BT.709 tags
  no longer reconfigure the presenter.
- Secondary Deck monitor windows now paint a neutral waiting placeholder before
  their first frame instead of a black surface; retained frame replay remains
  the preferred path when a frame is already available.
- macOS Pier now installs a Core Audio HAL input driver named `Arcen Microphone` and, when `microphone_input.enabled` is true and the driver Mach service is reachable, accepts Deck microphone-v1 Opus/PCM frames into that virtual 48 kHz mono Float32 input device.
- Windows Pier now makes the display HDR toggle follow the served pipeline:
  HDR sessions enable HDR on the exact capture targets, SDR pipelines disable
  it there before capture, and session/journal recovery restores the user's
  original state.
- Windows Pier NVIDIA headless HDR sessions now arm one recovery journal before
  HDR and exact timing changes, record temporary NVAPI timing ownership in that
  journal, and only record the held display after HDR is active and the exact
  mode is re-applied.
- Packaged Pier config templates now enable QP maps for Auto and Speed
  (`{"auto":"on","speed":"on"}`) while the built-in default remains off and
  existing installs keep their local config.
- macOS Deck `dev-tools` builds now support an env-gated windowed
  multi-monitor live-session test mode. `ARCEN_DECK_WINDOWED_MONITORS=2|3|4`
  requests a real multi-monitor layout from the Pier but presents each
  negotiated monitor as a decorated, resizable, non-fullscreen tiled window on
  one local display; production builds do not compile the mode.
- macOS Deck multi-window sessions now replay retained secondary frames when a
  secondary window becomes ready, aspect-fit secondary video with black
  letterboxing while routing input through the fitted image rect, and wrap
  display mismatch notices inside narrow viewer windows.
- macOS Deck multi-window input now ignores secondary-window clicks that start
  in letterbox bands while still delivering releases for drags that began
  inside the fitted image.
- macOS Deck windowed multi-monitor test mode now keeps the root window's
  stable title so native video layer lookup still works, and release packaging
  refuses any binary carrying the `dev-tools` marker even on `--no-build`.
- Pier configuration now accepts `video.qp_map` as either one
  `off`/`neutral`/`on` policy or a per-served-pipeline object keyed by Auto,
  Speed, Grading, HDR, Software and Custom. Linux and Windows pass the
  effective served-pipeline policy to capenc; macOS accepts the key and logs
  that VideoToolbox has no QP-map support.
- Software-only Pier refusals for Grading/HDR now carry the shared user-facing reason to the Deck instead of collapsing to a generic capture/encoder setup failure, while internal init failures remain generic.
- Linux multi-monitor sessions now aggregate the served pipeline across every started monitor encoder, so an NVENC primary with an OpenH264 fallback secondary reports and runs the Software contract instead of advertising the primary hardware preset.
- macOS Pier multi-monitor sessions now apply served-pipeline encoder bitrate start/ceiling per region and run one adaptive session rate controller from QUIC path signals, pushing live bitrate targets up to each region's operational contract ceiling without forcing idle re-encodes.
- The macOS Deck now latches native 8-bit and 10-bit presenter failures to the
  RGBA fallback for the rest of the session, and keeps recovery requests armed
  only until a non-empty RGBA fallback frame is handed to the existing texture
  path, so occluded windows do not keep idle streams polling forever.
- The macOS Deck now presents Grading and HDR root sessions through the same dedicated presenter-thread model as Auto/Speed instead of driving native video from egui repaint passes. Grading remains native `xf44` HEVC Main 4:4:4 10 BT.709 SDR into an RGB10A2 Metal layer with EDR off; HDR remains PQ/BT.2020 into that layer with EDR/HDR10 metadata only when the host confirms PQ.
- HDR now keeps the conservative link-safe start but owns a 500 Mbit/s contract
  ceiling through the same shared encoder-ceiling path as Grading, plus an
  evidence-gated fast probe and HDR-scoped host/Deck queue budgets so other
  pipelines keep their existing limits.
- NVENC sessions now initialise the encoder with the resolved session frame
  rate instead of a nominal 60 fps, so 30 fps Linux and Windows sessions deliver
  their configured bitrate target rather than roughly half of it.
- Speed now keeps the proven safe 1080p30 bitrate start but owns the true
  60 fps clean-path ceiling in the shared pipeline contract. Hosts derive
  Speed's motion priority and one-frame low-latency encoder/decoder policy
  from the served pipeline contract: NVENC reports P4/ultra-low-latency,
  zero reordering, no lookahead and a one-frame VBV; macOS ScreenCaptureKit
  captures at 1/60 for Speed requests; and the Deck applies VideoToolbox
  real-time decode for Speed sessions without treating optional decoder tuning
  as fatal.
- Auto, Speed, Grading and HDR are now named in the authenticated video request
  and echoed in `server_hello` as the active pipeline. The shared media crate
  owns one data contract per pipeline (codec ladder, colour target, bitrate
  bounds, keel policy and degradation text), while legacy clients still resolve
  through the previous request-field inference.
- Grading keeps the conservative link-capped starting bitrate but now has a
  shared 250 Mbit/s fidelity ceiling. The host adapters carry an optional
  served-pipeline encoder ceiling into capenc generically, so future fidelity
  pipelines can reuse the same path. Linux and Windows NVENC pass the value as
  `maxBitRate` while sizing VBV from the active target; the macOS Pier keeps
  `AverageBitRate` on the active target and passes the ceiling through
  VideoToolbox `DataRateLimits`; Auto and Speed retain their previous encoder
  bitrate numbers on this branch.
- The Deck command line and smoke tools accept `--pipeline
  auto|speed|grading|hdr`, deriving the same request fields as the GUI presets
  and printing the host's active pipeline.
- Software fallback is now a governed VM pipeline: the shared Software contract starts CPU H.264 near 4 Mbit/s for 1080p30, lets the live rate controller climb toward an 8 Mbit/s-class ceiling, and both Linux X11/OpenH264 and Windows MF/OpenH264 helpers accept runtime bitrate updates. Speed requests are visibly served as Software/Auto at 30 fps; Grading and HDR are refused on software-only hosts.
- Windows NVENC (the H.264, HEVC and AV1 hardware path) now submits frames
  through Keel's idle cadence, like the Linux NVENC path: a new frame, a
  requested keyframe, one pipeline flush after a change, and otherwise a
  keepalive once a second. It used to re-encode the last frame at the full
  frame rate, so an idle desktop at 60 fps still cost about 4.8 Mbit/s. The
  gate (`arcen_keel::SubmissionGate`) moved from the Linux encoder into
  `shared/keel`, so both hosts run the same policy.

### Fixed

- macOS Deck sign-in hand-over now keeps the actual retained 8-bit or 10-bit
  Metal presenter and its picture evidence alive across reconnect startup, so
  the last login-window frame stays visible until the user's desktop sends a
  replacement frame.
- Linux Pier installer upgrades now use the shared certificate provisioning
  plan: only positively classified legacy Arcen self-signed host pairs are
  reissued over the existing key with existing SANs, shared ownership marker
  and pin files; operator self-signed or CA-issued PEM and mismatched markers
  are left untouched. Linux installer issuance also shares the helper's
  certificate lock, journal, backup and rollback publication model.
- Windows Pier no longer warns "writer stopped with error" when the Deck quits
  normally and the broker has already closed the agent IPC WebSocket; resets
  and timeouts still warn.
- macOS Deck: dedicated 8-bit and 10-bit CAMetalLayer presenters now apply
  fullscreen/resize geometry on the main thread, update backing-scale drawable
  size, and replay the retained frame after geometry changes so native video
  fills the resized viewport.
- macOS Deck: session windows now keep an opaque black Core Animation backstop
  below dedicated native video, so any transient layer gap shows black instead
  of the desktop behind the transparent window.
- Windows remote unlock no longer always waits the full 15 s post-login
  stability ceiling: after an exact console bind, the broker can launch early
  and the per-session agent waits until the target session's input desktop is
  `Default` before starting capture. Repeated exact-bind and WTS-topology
  diagnostics are deduplicated.
- The macOS Deck now reports raw host-clock frame age as `wire_clock_age_ms`
  and uses offset-free `wire_delay_ms` (raw age minus the session rolling
  minimum) for overlays and stream-delay summaries, so inter-machine clock
  offset no longer looks like wire latency.
- Startup ramp no longer poisons session health with false critical FPS:
  shared QoS assessment suppresses host and client FPS during the bounded Pier
  warm-up window while keeping loss and latency checks active. Idle-desktop FPS
  classification remains a known gap until per-monitor damage evidence is
  wired into the health sample.
- Manual or graceful session shutdowns no longer log normal Deck disconnects
  as media-worker errors or Windows writer/audio-barrier warnings.
- macOS Deck now declares its Local Network privacy reason and, when LAN-only advisory probes report the host or network unreachable, shows a Local Network/offline-host hint while connecting and reports that specific reason if the normal connect deadline expires.
- Windows Pier debug logging now records incoming QUIC attempts before handshake completion plus startup network/firewall diagnostics and WTS session topology at boot/session changes.
- macOS Deck: transient dedicated-presenter drops or occluded skips no longer
  cover an already-established 8-bit or 10-bit native video layer with a black
  session background; fatal fallback statuses still switch to the egui path.
- macOS Deck: single-window sessions now re-enable display-refresh pacing on
  the root presentation layer while preserving immediate root presents for
  multi-window sessions, reducing input-storm pressure on WindowServer.
- macOS Deck: session telemetry now reports per-interval dedicated-presenter
  transient drawable drops and occluded skips as `presenter_drops` and
  `presenter_skips`.
- Linux Software Pier: live OpenH264 frame-rate sync no longer forces an IDR,
  avoiding once-per-second keyframe pops during Detail-priority telemetry.
- Linux CUDA/NVENC AV1 now repeats the AV1 sequence header on forced
  keyframes, matching Windows NVENC so recovery IDRs are classified as
  self-contained instead of leaving the Deck waiting.
- macOS Deck reconnect and resume overlays now preserve the last dedicated
  8-bit or 10-bit native video layer picture instead of flashing black while
  the replacement session waits for a fresh keyframe.
- macOS Deck root-layer pacing and colour tagging now positively avoid both
  dedicated video layers, only keeps transparent backgrounds after the current
  native layer has presented a picture, and counts every presenter drop/skip
  even when status notifications coalesce. Session telemetry also reports
  `ui_layout_passes_per_s` beside `ui_passes_per_s`.
- macOS Deck native-only reconnect/resume now leaves the recovery overlay as
  soon as the replacement session publishes its first native video frame,
  instead of waiting for an egui fallback frame that never arrives on the
  dedicated 8-bit and 10-bit paths.

- Windows Pier: a session no longer fails with "expected client_hello during
  handshake, received path_signal" when the Deck is slow to send its hello.
  The service starts forwarding live path signals to the session agent as soon
  as it relays; the agent's handshake reader now passes over that service-only
  message instead of refusing the session (seen as "Connection reset without
  closing handshake" on the Deck).

- macOS Pier: the 8-bit low-latency VideoToolbox encoder (Auto and Speed)
  does not report whether it is hardware-accelerated, so every macOS
  Auto/Speed session was served as `custom` and the Deck showed a pipeline
  degradation. The low-latency session is now created requiring a hardware
  encoder, which proves its class; if no hardware encoder is available it is
  created without the requirement and classified from its own read-back.
  Found in the lab end-to-end run.
- Deck CLI: `media-smoke` prints the host's authoritative `served_pipeline`
  updates and reports the latest served pipeline in its summary.

- macOS Pier multi-monitor sessions now send the authoritative
  `served_pipeline` truth before the first region-video frame, aggregating every
  region encoder's hardware/software read-back through `shared/media`; the
  macOS Deck applies both the served pipeline and the reported backend so the
  degradation badge no longer mixes software fallback truth with a provisional
  hardware hello.
- Windows Pier multi-monitor sessions now aggregate served-pipeline truth across
  every admitted monitor encoder instead of trusting only the primary monitor,
  so a hardware-primary/software-secondary set reports the software fallback.
- macOS Pier exact/custom single-monitor sessions preserve the absent requested
  pipeline through VideoToolbox encoder initialization, using Auto only for
  bitrate sizing and keeping the authoritative served truth `custom`.
- Linux, Windows and macOS Piers now size their operational rate controller
  from the pipeline actually served rather than the pipeline requested: HDR
  degraded to Grading uses Grading bounds, exact/custom streams keep legacy
  shape-derived bounds, and software fallback uses the Software contract.
- macOS Pier now reapplies the authoritative served contract after VideoToolbox
  reports the real encoder acceleration, updating the initial encoder bitrate
  and the controller before the first frame; multi-monitor region encoders apply
  the aggregate served contract as well.
- Windows Pier: a host whose display belongs to a virtual display adapter
  (for example a GeForce PC with a third-party virtual display driver) streams
  again. Every single-display NVENC session first asked NVIDIA to rebuild the
  display to the Deck's size; NVAPI does not drive such a display, so the
  session agent failed and the Deck saw only "Connection reset without closing
  handshake". When NVIDIA refuses before anything on the host has changed, the
  session now serves the existing display at its nearest mode. A failure while
  displays are being changed still refuses the session.
- Windows Pier: every failure before the session agent is ready now reaches
  the Deck as a reason instead of a reset connection.
- Windows Pier: the display recovery snapshot no longer requires NVAPI to
  know every output an NVIDIA GPU renders. A virtual display adapter's
  monitor is recorded as an ordinary Windows output, so the Pier may change
  its mode instead of refusing the session.
- Windows Pier: NVIDIA EDID provisioning is attempted only on Quadro, RTX Pro
  and GRID GPUs. A GeForce answered `NvAPI_GPU_SetEDID` with
  `NVAPI_NOT_SUPPORTED`, its rollback failed the same way, and every later
  session found an unrestorable recovery journal. A GeForce host is now
  served on its existing display without touching EDIDs.
- Windows Pier: a session no longer ends with "display restore failed" and an
  armed recovery journal when the display driver no longer offers the mode the
  display started in (a virtual display's default mode, or a mode list edited
  during the session). Windows refuses that mode every time, so the display is
  left at its current mode. Restores that also undo NVIDIA or VMware state are
  unchanged.
- Windows Pier: the capture log reports how often Desktop Duplication saw the
  pointer move, which shows whether a display draws the pointer into the
  picture (and a Deck drawing its own pointer then shows two).
- Windows Pier: when Desktop Duplication is refused, capture waits up to five
  seconds for it (the secure desktop shown right after a sign-in is usually
  brief) instead of settling at once for a WGC capture that may never deliver
  a frame, and the log names the input desktop it found, so a black picture
  caused by a lock screen or a pending UAC prompt says so.

### Performance

- The macOS Deck now has a dedicated root-window Auto/Speed presentation path:
  VideoToolbox's 8-bit 4:2:0 IOSurface-backed `CVPixelBuffer` is wrapped as
  Metal plane textures and drawn into a separate display-synchronised
  `CAMetalLayer`, so ordinary 8-bit video frames no longer require a CPU
  `CVPixelBuffer`→RGBA copy, `queue.write_texture` upload, or egui repaint.
  The existing egui/wgpu upload path remains the visible fallback, and
  secondary monitor windows still use the old path.
- The macOS Deck no longer runs its UI flat out on a still desktop. A session
  ran a full UI pass every 16 ms, and the media worker woke the UI again for
  every media batch, about 50 times a second for audio alone. Each pass
  repaints the whole window, so a full-screen Deck on a 3024×1964 panel showing
  a 1 fps idle stream measured about 48% CPU plus about 53% in WindowServer.
  With the window hidden, eframe skips the paint but not the pass, and the
  Deck spun at about 95%. The session now polls every 250 ms when idle, and
  every 16 ms for one second after local input or during a reconnect.
  Decoded frames, UI-affecting host results (cursor shape and mode, tablet
  mode, display updates), and tablet and gesture samples each wake the UI
  themselves.
- A healthy session no longer rebuilds its reconnect identity on every UI
  pass. The identity enumerates every display through WindowServer (about
  0.4 ms in the Deck plus about twice that in WindowServer per pass), and the
  auto-reconnect controller exists for the whole session, which also kept the
  idle poll above at 16 ms. Both now apply only while a resume is under way.
- `session telemetry` now carries `deck_cpu_percent`, `ui_passes_per_s` and
  the last `wire_video` header (codec included), so a Deck log shows what each
  session costs the Mac it runs on.

## [0.14.0] — 2026-09-28

### Configuration

- One set of defaults on every host, with everything on: the Linux, Windows
  and macOS installers write identical common sections (10-bit video, Opus
  audio, microphone input, clipboard both ways, time-zone redirection), and
  `check_shared_contracts.py` fails if they drift. The file is for turning
  things off. Multi-display is on where the host can prove it (macOS always,
  Windows by startup selection from live DXGI/NVAPI inventory, Linux by
  service-start NVIDIA head discovery). Existing configurations are kept on
  upgrade.
- Windows: multi-display no longer depends on an installer-time allow-list
  probe. Empty `platform.multi_monitor.allowed_adapters` means any eligible
  NVIDIA/NVENC adapter, `excluded_adapters` reserves GPUs such as an RTX card
  for other work, and NVIDIA headless provisioning is automatic unless an
  administrator forces it on or off.
- Linux multi-monitor now discovers NVIDIA `DFP-N` heads automatically from a
  short Xorg probe and ranks them by maximum pixel clock. Empty
  `platform.multi_monitor.heads` means automatic discovery; a non-empty list is
  an administrator override, and `advertise_enabled: false` remains the off
  switch.
- Windows: time-zone redirection works; the installer now creates the
  recovery directory its crash journal needs.
- macOS: the unused `virtual_display_enabled`, `native_login_enabled` and
  `privileged_broker_enabled` settings are no longer written, and no longer
  log a misleading "refusing session admission" warning.

### Shared rule, enforced

- `scripts/check_shared_contracts.py` (CI and pre-commit) fails when a Pier
  stops using a shared policy it must use, when a known host-local copy of a
  shared policy reappears, or when new portable code (no OS API) lands under
  `hosts/`. `check-workspace-boundaries.sh` works again and proves the
  dependency direction with a reviewed allowlist of product helper crates.
- Every Pier drives the shared session lifecycle, and every installer the
  shared install transaction.
- The Linux and Windows video queues share one drop / keyframe-recovery
  policy; macOS uses the shared audio priority stream.
- Every host takes the Deck display's colour from the shared rule; a bright
  display without HDR headroom is no longer counted as HDR.

### Installers

- An install succeeds only when the Pier is proven running: the Linux and
  Windows installers exit non-zero when the service does not settle running,
  and the macOS package fails when the service does not listen or a signed-in
  user's agent does not start. A missing privacy approval is onboarding, not
  a failure.
- Linux: after an upgrade without `--restart`, the running Pier keeps
  accepting new sessions on its own build until it is restarted, instead of
  refusing every login.

### Streaming

- macOS Pier now honours `redirection.timezone` without adding a second helper: the existing Agent Helper validates the Deck IANA time zone against zoneinfo, sets `TZ` in its launchd GUI session so newly launched apps inherit it, and restores the previous value from a per-user crash marker.
- The encoder bitrate follows the QUIC path on every Pier through one shared
  controller (`arcen_media::rate_control`). A random loss floor on Wi-Fi,
  cellular and VPN paths is not counted as congestion, a congestion epoch
  cuts once rather than every second, and a multi-display session is
  measured across all of its monitors and reaches every encoder pipeline.
  `ARCEN_RATE_CONTROL=0` restores the fixed rate.
- Presets are stated trade-offs, not promised frame rates: when an encoder
  cannot sustain a preset's ceiling (for example Speed at 60 fps on two
  displays of a vGPU), the session is admitted at the best proven rate and
  says so (`degradation_reason: fps_reduced_by_encoder_capacity`) instead of
  being refused.
- Every Pier now uses QUIC's BBR congestion controller, which does not read
  random loss as congestion. On a VPN path with a few tenths of a percent of
  random loss it gave each of two displays 2.5–3× the bitrate at half the
  latency of Cubic, and it matched Cubic on a calm path.
  `ARCEN_QUIC_CONGESTION=cubic` restores Cubic (Quinn marks its BBR
  experimental).

### Match my displays

- Deck: with several displays the remote pointer could bounce between
  displays about a thousand times a second (the Windows Pier lost the mouse,
  the Linux Pier lagged); a display now moves the pointer only when its own
  input carries a pointer event. A button released outside the display it
  was pressed on is still delivered.
- Windows Pier: the same user reconnecting takes over the session at once
  instead of waiting out the resume window; a different user is still
  refused. A new session waits for the previous one's display restore to
  settle, and every session end rolls back the NVIDIA headless outputs it
  provisioned.
- macOS Pier: each session owns its virtual displays through a short-lived
  helper process, so repeated multi-display sessions keep working.

### Input

- Trackpad scrolling is precise end to end: phased point deltas on macOS,
  high-resolution wheel on Linux, fractional and horizontal wheel on Windows.
- Negotiated gestures (`gestures_v1`): the Deck captures magnify, rotate,
  smart zoom and swipe; the macOS Pier injects what macOS allows publicly and
  declines rotate without ending the session.
- macOS Pier: reports the cursor and tablet modes it applied.

### Performance

- Windows Pier: colour conversion runs on the GPU in every preset. Eight-bit
  capture goes to NVENC as a texture (conversion 19.5 ms → 0), and Grading and
  HDR convert FP16 scRGB in a compute shader (about 14 ms and 38 ms per frame
  → under 0.1 ms), with the stream truth unchanged. The CPU path remains an
  explicit, logged fallback.
- macOS Pier: no audio dropped on a calm path (was 14%); VideoToolbox runs
  with low-latency rate control.

### Displays

- macOS Pier: a Deck with several displays gets one virtual display per Deck
  display, arranged as the Deck's layout (including offset arrangements),
  with region input.
- Deck: a launch-time quick connect negotiates Match My Layout like the Home
  screen, and a saved connection's own Displays choice applies to it. The
  Deck's root window sits on its fastest display, and each display presents
  at its own refresh. Known issue: with several displays the Deck's draw loop
  runs far more often than frames arrive, which costs CPU.

### macOS Pier installer

- The first page says what is installed and why it is more than one app: a
  network service running as the hidden `_arcen` account, and a helper in
  each signed-in session. The full reasoning is public in
  `docs/security/macos-pier-process-model.md`.
- The Agent Helper moves to `/Library/PrivilegedHelperTools`, so
  Applications shows one Arcen app. The package no longer asks for a
  destination or install location.
- Both launchd jobs declare `AssociatedBundleIdentifiers`, so System
  Settings attributes them to Arcen Pier.
- The helper asks for its three approvals as soon as it starts, one at a
  time: system audio, Accessibility, then Screen Recording. Previously
  Accessibility opened underneath Screen Recording, and system audio waited
  for the first Deck.
- Declining "Installer would like to administer your computer" (the
  `_arcen` account) stops the install with a reason instead of leaving a
  service that cannot start.
- `uninstall.sh --purge` also resets the privacy approvals given to Arcen.

### Fixed

- Deck: *Forget and Verify* on a changed host identity reconnects at once and
  shows the new fingerprint, reusing the password already given, instead of
  returning to Home.
- Deck: a host's reason for closing a session is shown rather than a bare
  "Host closed the session"; the macOS Pier sends one when it refuses a
  session after sign-in (for example while system audio is unapproved),
  instead of resetting the connection.
- Deck: a host that accepted the sign-in but could not start the session says
  so ("the host signed you in but could not start the session") instead of
  "authentication failed" (`AuthResult.session_setup_failed`).
- macOS Pier: advertises its build identity like every other product, from
  one shared `arcen_protocol::build_identity`.
- Linux installer: an installed but stopped firewalld is reported as such,
  not as a failed firewall update.
- macOS Pier: the relay no longer sends path signals to the agent before the
  Deck has authenticated, which could fail the handshake; a Deck can no
  longer send service-only messages to an agent on macOS or Windows.
- Windows installer: waits for the service as long as the service control
  manager does (30 s) before reporting a failed install.
- Windows Pier: the adaptive bitrate task stops with its attachment and
  follows replaced pipelines; a fatal attachment cleanup still ends the
  session's shared lifecycle.

### Linux Pier

- On a desktop declared `rec2100-pq`, Grading is converted to true BT.709 SDR
  and eight-bit presets are refused with a clear message.

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

- Windows: an upgrade needs no flags and no second run. A self-signed key
  pair from an older install is taken over automatically on every host,
  keeping the key (a CA-issued certificate is served as it is and never
  reissued); an access entry Explorer added to `ProgramData\Arcen` is removed
  instead of failing the install; uninstall while the sign-in screen still
  has the credential provider loaded sets it aside and finishes; the
  transcript shows what changed, with command output behind `--verbose`.
- Windows: a host without NVIDIA (VMware, Citrix, Proxmox and other VMs)
  serves every preset as OpenH264 H.264 4:2:0 8-bit instead of failing the
  session after sign-in, and a failure before streaming reaches the Deck with
  its reason instead of as a dropped connection.
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

[0.15.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.15.0
[0.14.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.14.0
[0.13.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.13.0
[0.10.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.10.0
[0.9.8]: https://github.com/Aanerud/arcen_public/releases/tag/v0.9.8
