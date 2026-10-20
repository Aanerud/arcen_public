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
- A dedicated Xorg desktop's depth is decided once, when it is created
  (`arcen_media::video::choose_desktop_depth`, `video.x11_desktop_depth`):
  30 only for a ten-bit creating session, a declared PQ desktop or a pin,
  24 otherwise, because Firefox and Chromium lose the GPU at depth 30. The
  launcher rewrites only the template's packaged depth lines
  (`arcen_outputs::with_session_screen`) and reports the live root depth;
  a reattachment is served from that depth (`serve_on_fixed_depth`: a
  ten-bit request on a depth-24 desktop is visibly Auto,
  `eight_bit_source`, with `EIGHT_BIT_DESKTOP_DETAIL` for the Deck). Never
  replace a desktop to change its depth, and never serve eight-bit samples
  as a ten-bit contract.
- Xorg depth 30 is genuine precision but not an HDR composition contract. PQ
  and HLG requests must resolve to Grading BT.709 SDR until a color-managed
  Wayland provider proves transfer, primaries, metadata, and capture format.
- XShm cannot composite the host cursor. Resolve Host to Local before native
  preflight and live spawn; keep Host unchanged on the eight-bit NvFBC path.
- READY and session truth must identify `capture=nvfbc
  capture_zero_copy=true` or `capture=xshm capture_zero_copy=false`.
- A single-head session's head takes the served Deck display's EDID: the
  launcher generates it with `arcen_outputs::edid::generate` from the facts in
  the open request and names it with `CustomEDID` beside `ConnectedMonitor`.
  Never pass EDID bytes across the launcher boundary; pass facts. A display
  the generator refuses keeps the EDID-less head rather than failing.
- Capture reports what it encoded: capenc logs `encoded stream truth` from
  each new HEVC SPS, to compare with the Deck's `received stream truth`.
- Never drop an encoded P-frame to relieve a full send queue while waiting
  would do: the frame pump waits for room (`FrameQueue::wait_for_room`,
  bounded by `ROOM_WAIT`) so capenc slows instead of breaking the prediction
  chain. Drop-and-IDR is only for a link that stays blocked past the bound.
- Every session runs on the shared isolated QUIC lanes (ADR 0014,
  `arcen_transport::quic::session_lanes`): one task per role in
  `net/lane_workers.rs`. Audio has its own lane and writer and never falls
  back onto video or control; video waits only for its own lane's room;
  input is injected by the input-lane worker. Do not reintroduce a shared
  sender loop.
- Bitrate is the shared `link_capped_average_bitrate_bps`; do not size NVENC
  from a local formula.

## X11 virtual input

- The dedicated Xorg admits only its own session's uinput devices: name them
  with `arcen_input::virtual_device::SessionInputIdentity` and keep the
  launcher's `with_session_input_isolation` classes on every rendered
  configuration. Never add another input device to a session's X server, and
  never let console devices reach a remote desktop.
- Never emit `KEY_SYSRQ` from a kernel virtual keyboard
  (`input/keymap.rs::kernel_safe_key`).

## Headless Wayland session (`wayland-provider`, default off)

- Each desktop is its own seatless logind session (`Type=wayland`,
  `Class=user`) opened by the launcher through PAM; never start a compositor
  from the Pier service, an operator shell or another login.
- A native desktop (`wayland.session_mode`, `arcen_session::native_seat`)
  is the one exception to "seatless": its session opens on its own logind
  seat that the Pier builds (NVIDIA card node, `vkms`, the desktop's
  tablet) and takes apart again, all volatile under `/run`. Devices reach
  its compositor only through logind `TakeDevice`; never grant ACLs, and
  never move a card that is open off `seat0`.
- The compositor, private D-Bus bus, PipeWire and WirePlumber run as the
  session user in `/run/user/<uid>/arcen-wayland-<session>`. WirePlumber
  never opens hardware and keeps its state there. The session's audio is its
  own graph (`session/wayland/audio.rs`): a session sink from an
  `XDG_CONFIG_HOME` drop-in plus a private `pipewire-pulse`, or, on a
  PulseAudio host, pulse tunnels into the user's server; the choice is
  `arcen_session::local_playback::resolve_wayland_audio` from host facts.
- `audio.local_playback` is a session-long lease held by the session agent
  (`media/local_playback.rs`); decisions and evidence are
  `arcen_session::local_playback`'s. A required mute that is not proven
  refuses the session; never weaken that to a warning.
- Do not grant DRM, input or render-group access to make a compositor
  "work": no `/dev/dri` permission changes, no group additions, no
  SELinux/Secure Boot/kernel changes. GNOME 40 runs surfaceless.
- The agent assembles the GNOME login itself (`session::wayland::session_services`,
  policy in `arcen_session::desktop_session`): it serves
  `org.gnome.SessionManager` on the private bus with `SessionIsActive=false`,
  `CanShutdown=false` and no logind `IdleHint`, unlocks the keyring with
  the login password handed through the agent's stdin (never argv, env or
  a file), and runs the settings daemons and XDG autostart as optional
  members. Do not start `gnome-session` (GNOME 49 runs it only under the
  user's systemd manager, outside the scope and private bus), and never
  run a host-scoped autostart entry (time zone, radios, sharing, USBGuard,
  modem, VM console agents). Every service is optional; none is a
  readiness step.
- Every desktop, on either provider, holds a logind `sleep:idle` block
  inhibitor in the launcher (`session::sleep_inhibitor`) for its whole
  life. Deck keys must never map to host power, sleep, suspend or wake keys.
- Readiness is outputs + stream + input target + a real frame
  (`arcen_session::wayland_session`), never a socket or a running process.
- The capture hand-off is `arcen_media::video::capture_handoff`; do not add
  ad-hoc `ARCEN_CAPTURE_*`/`ARCEN_PIPEWIRE_*` variables.
- The clipboard of a Wayland session goes only through that session's
  `arcen-clipboard.sock`, proven like the input socket. The agent drives
  Mutter's `RemoteDesktop` clipboard on a session of its own and never
  disables it (GNOME 40–50 cannot re-enable one). Every clipboard decision
  is the shared `SelectionBridge`'s, and the format table is shared with the
  Xorg child.
- Input into a Wayland session goes only through that session's
  `arcen-input.sock` (`input::wayland::WaylandInjector`, proven by owner,
  peer pid and `hello` facts). Never uinput or XTest there: they reach the
  physical seat. Coordinates carry the output generation. capenc runs its
  Wayland Auto/Speed compositions (`hosts/capenc/src/wayland/pipelines/`)
  only when that hand-off is in its environment; `wayland-provider` turns on
  `arcen-capenc/wayland` (links `libpipewire-0.3`).
- Pen in a Wayland session is only an emulated stylus on the compositor's
  EIS seat (libei `ei_stylus`), probed by the agent at start
  (`session::wayland::eis`, pure-Rust `reis`) and reported in the input
  `hello` as `StylusFacts`; advertise pen only from a bound stylus, and
  plan its frames with the shared `arcen_input::pen_emulation`. The one
  exception is the lab seat pen (`wayland.lab_seat_pen`, default off,
  `input::seat_pen`, shared `arcen_input::seat_pen`). It is one uinput
  tablet per desktop on that desktop's own udev seat, never seat0, for a
  compositor built with the headless-input lab patch. Owned by the desktop:
  never share one device, seat or grant between desktops, route pen samples
  only to the attachment's own desktop, and revoke, destroy and remove the
  rule when it ends. Its per-desktop-user ACL on that one node is pending
  Release/Security review against the input-access rule above; never enable
  it by default. Hard USB
  and raw HID are refused for every Wayland session
  (`arcen_usb_bridge::admit_hard_usb_session`): a headless compositor opens
  no input devices, so a bridged device would reach `seat0`.
- Committed text into a Wayland session is typed only where the session
  proves it: the agent reads its compositor's keymap as a Wayland client
  (request `wl_keyboard` only after the seat advertises the keyboard
  capability; earlier crashes headless Mutter 49.4), indexes it with
  libxkbcommon loaded at run time, and types by keysym only what Mutter's
  active-group rule will type; the rest goes through the compositor's
  `ei_text` when its EIS seat offers it, or is refused and reported to the
  Deck. Never log committed text, only counts. The agent keeps one
  long-lived EIS connection for text (`session/wayland/eis_text.rs`); the
  stylus probe (`session/wayland/eis.rs`) is short-lived, and a bound
  stylus should share the text connection rather than open another.
- A Wayland capenc states its admission (`[capenc] ADMISSION …`, the shared
  `CaptureAdmissionNotice`) before `READY`; the Pier accepts a lower-rate
  `READY` only through that notice, and takes the first-frame readiness
  step from it. Speed on a copying route without a qualified rate is
  visibly served as Auto.
- DMA-BUF reaches capenc only on the verified `gpu-convert` route
  (`hosts/capenc/src/wayland/gpu_convert.rs`: EGL image → GL blit into a
  CUDA-registered texture → CUDA copy; same GPU only; `PipeWire` explicit
  sync honoured). The Pier passes `gpu-import=verified` only when the
  qualification record proves it with frames read back equal to a host
  copy; never classify it as zero-copy, and never hand capenc a DMA-BUF
  route the record has not verified.
- A session created for Speed runs its virtual output at the lowest refresh
  the record holds Speed at (the display's own on Mutter 49's virtual
  outputs; the shared `paced_output_refresh_hz` only where that alone holds);
  Auto keeps the Deck's refresh. Speed is decided only by the shared
  `resolve_speed` (measured, or `inferred` from a larger output of the same
  refresh, codec and route — say so); never assume a rate.
- Every served Wayland Speed epoch is watched by the shared
  `SpeedRuntimeGuard` on capenc's own delivered/encoded counts; a shortfall
  re-serves the attachment as Auto (`speed_runtime_below_floor`). Never judge
  capability on Deck-side or network numbers.
- A stale record (version change only) is re-qualified by the service at
  start (`requalify_at_start`); a missing or untrusted record never is.
- A desktop fixes its streams' `cursor-mode` when it opens
  (`resolve_stream_cursor_mode`, kept through every reconfiguration):
  `metadata` for a Deck that wants a local cursor and declared
  `cursor_image_v1` on a host whose record proves `cursor_metadata`,
  `hidden` (`cursor=local`, the Deck's own pointer) for any other Deck that
  wants a local cursor, `embedded` (`cursor=host`) only for a host-cursor
  request. Never composite a cursor the Deck also draws. A metadata desktop's capenc writes
  framed-v2 cursor events; the Pier keeps the desktop's cursor
  (`media::wayland_cursor`, outliving attachments because Mutter sends a
  sprite only when it changes) and sends `CursorUpdate` images on the input
  lane. Never start the X11 cursor watcher for a Wayland session. The Pier
  sets `cursor=` from the hand-off.
- The provider is chosen at start from facts (`capture.provider = auto`):
  an installed Xorg server keeps X11 and is never migrated; without Xorg,
  Wayland needs a DRM render node and a valid qualification record
  (`arcen-pier wayland qualify --write`, `/var/lib/arcen/`); otherwise the
  Pier refuses to start and says what to install. Never change driver
  configuration, and never decide by distribution name.
- A Wayland session serves Auto (Speed only on a qualified rate). Grading
  and HDR are served only when the record's fidelity proof says so (Mutter
  ≥ 50 on the `force-hdr` route: the session's compositor alone gets
  `MUTTER_DEBUG_FORCE_HDR`, never `--debug-control`); otherwise HDR falls
  back to a proven Grading, else Auto, visibly, never failed. Precision is
  proven by a read-back ramp per format and memory, never assumed from an
  offered wide format (Mutter's shm screencast paints eight-bit).
- A Wayland host without a usable NVENC still streams: `encoder=software-h264`
  runs capenc's separate `linux-wayland-software` composition (shared memory
  only, the shared `SoftwareH264Session` and Keel-gated loop, no CUDA).
  Selection never requires NVIDIA, only a render node a session can open;
  the qualification probes the native encoder and records
  `facts.encoder.native`, and a host without one sizes each session's
  output to OpenH264's limits before it exists and asks for software
  directly; on such a host a `display_update` is fitted too, and an attach
  fits an oversize resizable output before capture. A native host's runtime
  software fallback never resizes the output under it: it refuses an
  oversize one by name.
- Outputs follow the compositor's version (`wayland_outputs::VirtualOutputMode`):
  Mutter 49+ uses `RecordVirtual` monitors created by negotiating their
  exact size (`arcen_capenc::materialise_virtual_output`), placed with
  `ApplyMonitorsConfig` and changed only through the agent's
  `arcen-output.sock` output transaction, one output generation at a time.
  Stop every capture epoch of the session before reconfiguring it; capenc
  always offers exactly the hand-off size. Older compositors keep fixed
  `--virtual-monitor` outputs. An output count change adds streams to the
  running ScreenCast session (`ScreenCast.Stream.Start`) or stops them;
  each output is placed at the UI scale `arcen_outputs::choose_output_scale`
  picks from the client display's density.
- Multi-monitor on Wayland is offered from the compositor's output mode and
  the record's proven concurrent outputs (`facts.multi_output`), never from
  NVIDIA heads; topologies are planned with the shared
  `arcen_outputs::virtual_topology`, each output gets its own capenc and
  stream hand-off (`MonitorCaptureBinding`), region input lands on the
  output that shows the region, and a reattach with another layout moves
  the outputs in place before the first capture epoch.
- A capture epoch that ends on its own is restarted inside the attachment
  (`CaptureRestartBudget`); never end the session for it, and never loop
  past the budget.
- X11 applications run through the compositor's own Xwayland
  (`platform.session.wayland_x11_apps`, resolved once at start by the shared
  `resolve_x11_apps`): never start an X server, never put `DISPLAY` or
  `XAUTHORITY` in the Pier's Wayland session environment (Mutter exports
  them to applications), and keep the cursor watcher, deskside, X capture
  and the X11 clipboard child off. Leftover X11 sockets and locks are
  removed only through `xwayland::remove_session_artifacts` (directory
  descriptor, `fstatat` without following, the session's own displays and
  compositor pid), never by path.
- Lab proof: `examples/wayland_session_harness.rs` (`--x11-client`,
  `--x11-glx` for Xwayland); see
  `docs/architecture/linux-wayland-provider.md`.

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

The host's X11 seat is a second, host-wide limit: one X11 desktop per host.
A dedicated Xorg desktop is a logind session on `seat0` at VT 1, and logind
refuses a second one ("already occupied", which `pam_systemd` turns into a
PAM success without `XDG_SESSION_ID`). The launcher probes the occupant
(shared `arcen_session::x11_seat`) and answers `seat_occupied`; the Pier
closes with `DECK_REASON_SEAT_OCCUPIED`. Every launcher failure after
authentication must write a response status; exiting without one surfaces
as the misleading "session launcher protocol failed".

A seated desktop is local to polkit (it never reads logind's `Remote`), so
Arcen's rule `05-arcen-remote-session.rules` (shared
`arcen_session::polkit_remote`, adapter `session/polkit_remote.rs`) gives
remote seated sessions polkit's `allow_any` answers, keyed on `REMOTE=1`,
`TTY=arcen` and `seat-arcen-pen-…` — keep `set_rhost` and `set_tty("arcen")`
on every launcher path. A seated desktop without the rule is refused before
launch; every desktop is proved with `pkcheck` before hand-over and refused
(`LauncherError::PolkitUnprotected`) on a grant (a seated one also when
polkit does not answer). A native Wayland desktop is seated
(`DesktopSeat::ArcenSeat`, `polkit_remote::desktop_seat(provider,
native_seat)`), never seatless; its seat is not built unless
`native_seat::polkit_blocker` passes. Never add a second polkit rule or
module for one product path. Site restrictions must sort before `05-`.

### capenc READY line

capenc emits `[capenc] READY version=1 backend=... codec=... cursor=local|host ...`
on stderr. The pier's `parse_ready_line` requires the `cursor=` field. The
`cursor=` value echoes the `cursor=local|host` argv the pier passes. Old capenc
binaries did not emit this field and will fail with
`capenc READY protocol error: missing cursor`.

stdout is framed-v1 (`u32_be length || access unit`) unless the Pier passes
`framed-v2` (a Wayland metadata-cursor capture): then every record is
`u32_be length || u8 kind || body`, kind 1 an access unit and kind 2 a
cursor event (`arcen_media::capenc_stream`, bounded before allocation).
