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
NVIDIA GPU and driver on the build/test host. The software encoder also needs
`nasm` on the build host: without it OpenH264 silently builds without its
assembly and encodes three to four times slower. Run
`scripts/verify-openh264-assembly.sh target/release` on every release build.

These are the rules. Why each holds, and how the code meets it, is in
[`ARCHITECTURE.md`](ARCHITECTURE.md), "Design behind the Linux Pier's rules".

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
- Grading passes the depth-30 desktop's sRGB codes to the matrix untouched
  (`Rgb10Signal::Direct`). The transfer a Deck is told comes only from the
  shared `DesktopPlan::serving_sdr_desktop` (Xorg) and
  `AdmissionRequest::sdr_transfer` (Wayland); never state it locally. Never
  assume the piecewise sRGB curve for a compositor's SDR content: measure it.
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
  A canvas record waits for room however long the path stalls (shared
  `OutboundVideoRecord::room_wait`, `FrameQueue::enqueue_with_backpressure`):
  a dropped one costs a new canvas generation.
- Every session runs on the shared isolated QUIC lanes (ADR 0014,
  `arcen_transport::quic::session_lanes`): one task per role in
  `net/lane_workers.rs`. Audio has its own lane and writer and never falls
  back onto video or control; video waits only for its own lane's room;
  input is injected by the input-lane worker. Do not reintroduce a shared
  sender loop.
- Bitrate is the shared `link_capped_average_bitrate_bps`; do not size NVENC
  from a local formula.
- capenc's READY line must carry `cursor=` (`parse_ready_line` requires it);
  keep READY and the stdout framing in step with
  `arcen_media::capenc_stream` (ARCHITECTURE.md, "capenc READY line and
  stdout framing").

## X11 virtual input

- The dedicated Xorg admits only its own session's uinput devices: name them
  with `arcen_input::virtual_device::SessionInputIdentity` and keep the
  launcher's `with_session_input_isolation` classes on every rendered
  configuration. Never add another input device to a session's X server, and
  never let console devices reach a remote desktop.
- The one exception is a Native tablet (ADR 0021). It stays on `seat0`,
  tagged for one display only; the launcher appends, last and after the
  isolation classes, a class ignoring every `arcen_bridge` again and the one
  class that re-admits this display's own tag
  (`arcen_usb_bridge::bridge_seat::xorg_session_bridge_class`). Nothing may
  follow them. Never admit
  a bridged device by name, VID or a wider tag, never start a bridge before
  `usb_bridge::seat::x11_isolation` proves every running X server keeps it
  out, and never tell a user to sign out for it.
- A joined console's X server (ADR 0025) is not one Arcen configures. Each
  device is admitted there only while the shared proof
  (`arcen_input::joined_input`) holds, through the gate every injection
  passes (`uinput.rs` `GatedDevice`); the shared `JoinedInputGuard` decides
  when held input is released and input resumes, or the attachment ends.
  Never inject on a joined console around the gate. Who may read a device
  is the shared `classify_reader`; Linux reads only the facts. A reader is
  matched by the device number of what it holds, never by path. Never widen
  this by process name.
- Never emit `KEY_SYSRQ` from a kernel virtual keyboard
  (`input/keymap.rs::kernel_safe_key`), the committed-text path included
  (`input/x11_text.rs::CoreKeymap::evdev` never yields it, whatever the
  session's keymap puts at keycode 107).
- Never send key input (presses, or releases, which carry modifier and lock state) to the uinput keyboard while the foreground virtual
  terminal reads kernel keyboards (`input/console_guard.rs`: only `K_OFF`
  or no VTs admit keys); a text console in front would take a Deck's
  Ctrl+Alt+Del as a reboot. Every Arcen X server keeps `DontZap`
  (`session/launcher.rs::with_dont_zap`).
- Keys outside the main block come from the shared
  `arcen_input::keyboard::host_keys` table; F13–F24 and Menu are pressed as
  keys and never borrowed for committed text (`BORROWABLE_EVDEV`).
- The session's layout reaches the Deck as a `host_keymap` notice, built on
  both providers with `input/keymap.rs::host_keymap_msg` over the shared
  `arcen_input::text::host_layout` builder; never build it another way.

## Native tablet: one path

- `usb.native_tablet = "enabled"` is the only switch (the feature is
  `native-tablet`, default); never add a lab feature or environment
  variable that turns it on.
- Where the bridged pen points is ADR 0023's one path, for X11 and Wayland
  alike: `usb_bridge::tablet_pointer` reads the pen's own evdev node
  (read-only, never grabbed, `O_NOFOLLOW`, device number and seat rule
  re-verified after the open) into `media::desktop_pointer` (the shared
  `PenPointerHub`); the shared `PenPointerLead` decides when the pen leads.
  Never add a second pen reader or a platform-specific mapping.
- On X11 the cursor watcher reads the X pointer only when the pen does not
  lead (application warps, a remapped tablet, sessions without a bridged
  pen), and proves the mapping once a second from the pen's
  `xf86-input-wacom` devices (`x11_wacom_mapping`).
- The desktop's bridge class sets `RawSample` 1
  (`bridge_seat::XORG_BRIDGE_RAW_SAMPLE`); keep it, and keep `Suppress` at
  the driver's default.
- A joined console (ADR 0025) never takes the raw bridge: its X server is
  not one Arcen starts. It takes the host-decoded route
  (`net/joined_pen.rs`), offered only while the shared `joined_pen` proof
  holds. Never relabel a joined console as an X11 desktop, edit or
  restart its X server, or weaken the ignore classes to admit the raw bridge.
- Prove pen changes with a real tablet or a real browser, not only the
  emulated Intuos5; read `BTN_STYLUS*` before blaming timing ("pen but no
  ink" is usually a held side switch).

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
  own graph (`session/wayland/audio.rs`), chosen only by
  `arcen_session::local_playback::resolve_wayland_audio` from host facts.
- `audio.local_playback` is a session-long lease held by the session agent
  (`media/local_playback.rs`); decisions and evidence are
  `arcen_session::local_playback`'s. A required mute that is not proven
  refuses the session; never weaken that to a warning.
- Do not grant DRM, input or render-group access to make a compositor
  "work": no `/dev/dri` permission changes, no group additions, no
  SELinux/Secure Boot/kernel changes. GNOME 40 runs surfaceless.
- The agent assembles the GNOME login itself (`session::wayland::session_services`,
  policy in `arcen_session::desktop_session`). It hands the login password
  to the keyring only through the agent's stdin (never argv, env or a
  file). Do not start `gnome-session`, and never run a host-scoped
  autostart entry (time zone, radios, sharing, USBGuard, modem, VM console
  agents). Every service is optional; none is a readiness step.
- While the Pier's session-bound input connection is served (a Deck is
  attached) the agent's session manager holds the shared
  `DeckAttachedInhibit` idle inhibitor, released when the connection ends.
  Never hold it past the attachment, and never pin `idle-delay` or the lock
  settings instead (they are the user's).
- Every desktop, on either provider, holds a logind `sleep:idle` block
  inhibitor in the launcher (`session::sleep_inhibitor`) for its whole
  life. Deck keys must never map to host power, sleep, suspend or wake keys.
- The input probe also creates the session's keyboard
  (`MutterSessions::prime_keyboard`). Keep it ahead of readiness.
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
  EIS seat (libei `ei_stylus`, `session::wayland::eis`); advertise pen only
  from a bound stylus, and plan its frames with the shared
  `arcen_input::pen_emulation`. The one exception is the seat pen
  (`wayland.lab_seat_pen`, default off, `input::seat_pen`, shared
  `arcen_input::seat_pen`): one uinput tablet per desktop on that desktop's
  own udev seat, never seat0. Never share one device, seat or grant between
  desktops, route pen samples only to the attachment's own desktop, and
  revoke, destroy and remove the rule when it ends. Its per-desktop-user
  ACL on that one node is pending Release/Security review against the
  input-access rule above; never enable it by default. Hard USB
  and raw HID are refused for every Wayland session
  (`arcen_usb_bridge::admit_hard_usb_session`): a headless compositor opens
  no input devices, so a bridged device would reach `seat0`.
- A seat pen's pressure goes through the shared
  `arcen_input::compositor_pressure::SeatPenPressure`, chosen from the
  qualified compositor version, and
  `input::pressure_curve::SeatPenPressurePlan::resolve`; never write a seat
  pen's pressure linearly. Never turn the inverse off by version or
  configuration alone: a restored stock library must bring it back by
  itself.
- Committed text into a Wayland session is typed only where the session
  proves it. Request `wl_keyboard` only after the seat advertises the
  keyboard capability. Never type through Mutter's `NotifyKeyboardKeysym`:
  it presses the first key listing a keysym and drops higher-level
  modifiers. What the keymap cannot prove goes through the compositor's
  `ei_text` when its EIS seat offers it, or is refused and reported to the
  Deck. Never log committed text, only counts. Keep one long-lived EIS
  connection for text (`session/wayland/eis_text.rs`); a bound stylus
  should share it rather than open another.
- A Wayland capenc states its admission (`[capenc] ADMISSION …`, the shared
  `CaptureAdmissionNotice`) before `READY`; the Pier accepts a lower-rate
  `READY` only through that notice, and takes the first-frame readiness
  step from it. Speed on a copying route without a qualified rate is
  visibly served as Auto.
- DMA-BUF reaches capenc only on the verified `gpu-convert` route
  (`hosts/capenc/src/wayland/gpu_convert.rs`). The Pier passes
  `gpu-import=verified` only when the qualification record proves it with
  frames read back equal to a host copy; never classify it as zero-copy,
  and never hand capenc a DMA-BUF route the record has not verified.
- Speed is decided only by the shared `resolve_speed` (measured, or
  `inferred` from a larger output of the same refresh, codec and route —
  say so); never assume a rate. A session created for Speed runs its
  virtual output at the lowest refresh the record holds Speed at; Auto
  keeps the Deck's refresh.
- Every served Wayland Speed epoch is watched by the shared
  `SpeedRuntimeGuard` on capenc's own delivered/encoded counts; a shortfall
  re-serves the attachment as Auto (`speed_runtime_below_floor`). Never judge
  capability on Deck-side or network numbers.
- A stale record (version change only) is re-qualified by the service at
  start (`requalify_at_start`); a missing or untrusted record never is.
  A valid record whose X11 proof was skipped for a host precondition that
  holds again is re-proved by the same run (`requalify_x11_at_start`,
  shared `precondition_requalify_decision`): once per boot and stack,
  never refusing start, outcome typed (`X11RequalifyOutcome`). Never add
  another trigger without a shared decision and its own hold-off.
- A desktop fixes its streams' `cursor-mode` when it opens
  (`resolve_stream_cursor_mode`, kept through every reconfiguration).
  Never composite a cursor the Deck also draws. Never start the X11 cursor
  watcher for a Wayland session. The Pier sets `cursor=` from the hand-off.
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
- A Wayland host without a usable NVENC still streams through capenc's
  separate `linux-wayland-software` composition. Selection never requires
  NVIDIA, only a render node a session can open. A native host's runtime
  software fallback never resizes the output under it: it refuses an
  oversize one by name.
- Outputs follow the compositor's version (`wayland_outputs::VirtualOutputMode`)
  and change only through the agent's `arcen-output.sock` output
  transaction, one output generation at a time. Stop every capture epoch
  of the session before reconfiguring it; capenc always offers exactly the
  hand-off size. A resize never ends the desktop (`resize_recovery`).
- Multi-monitor on Wayland is offered from the compositor's output mode and
  the record's proven concurrent outputs (`facts.multi_output`), never from
  NVIDIA heads; topologies are planned with the shared
  `arcen_outputs::virtual_topology`.
- A capture epoch that ends on its own is restarted inside the attachment
  (`CaptureRestartBudget`); never end the session for it, and never loop
  past the budget.
- A lock never ends a desktop. When the shell inhibits remote access (its
  shield is up on a compositor that is not `--headless`), Mutter closes
  every RemoteDesktop/ScreenCast session; the agent keeps the desktop and
  suspends capture and input as the shared
  `desktop_session::remote_access::StreamsLifetime` decides, and records
  the outputs again (new clipboard session, EIS text reconnected) once
  Mutter allows it. Never end a desktop on a refusal Mutter attributes to
  the inhibition, and never capture a `hello` that says
  `streams_unavailable`: attachments wait through
  `output_client::current_live` (`attach_wait`).
- X11 applications run through the compositor's own Xwayland
  (`platform.session.wayland_x11_apps`, resolved once at start by the shared
  `resolve_x11_apps`): never start an X server, never put `DISPLAY` or
  `XAUTHORITY` in the Pier's Wayland session environment (Mutter exports
  them to applications), and keep the cursor watcher, deskside, X capture
  and the X11 clipboard child off. Leftover X11 sockets and locks are
  removed only through `xwayland::remove_session_artifacts` (directory
  descriptor, `fstatat` without following, the session's own displays and
  compositor pid), never by path.
- Proof harness: `examples/wayland_session_harness.rs` (`--x11-client`,
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

- The Pier holds a capacity-one session admission gate and keeps the slot
  reserved for the reconnect window (`auth.reconnect_window_secs`, 180 s by
  default, at most two hours). Do not remove or widen that gate without
  understanding what it protects.
- One X11 desktop per host: the launcher probes the seat's occupant (shared
  `arcen_session::x11_seat`) and answers `seat_occupied`; the Pier closes
  with `DECK_REASON_SEAT_OCCUPIED`. Every launcher failure after
  authentication must write a response status; exiting without one surfaces
  as the misleading "session launcher protocol failed".
- A host without a display manager gets the account's real login session
  (ADR 0025 §10, `session/login_session.rs`, shared
  `console_session::login_session`). Never tie its lifetime to
  a connection or to the Pier's process tree, and never authenticate with
  `arcen-login` (its stack denies `auth`). Only a complete read of logind's
  sessions proves the account has none (`console::read_sessions` returns a
  shared `console_session::SessionRead`): never turn a `loginctl` failure,
  timeout or partial answer into an empty list; decide with the `*_read`
  decisions, which refuse `console_sessions_unreadable`. Likewise one
  unanswered `show-session` is not a logout: a joined launcher watches its
  session through `console::watch_joined` (shared
  `console_session::session_watch`), never through `session_fact(..).is_none()`.
- The account's own GNOME Wayland console session is joined, never
  replaced (ADR 0025 section 9, S4b): the console agent's `--wayland-join`
  mode (`session/wayland/join.rs`) starts no bus, compositor or PipeWire and
  never changes the session's monitors. Every decision (outputs, desk
  privacy, the proved route, lock, departure) is shared
  `console_session::wayland_attach`; keep the launcher's
  `run_wayland_join` an adapter. A joined Wayland session is never held for a
  resume.
- Another product's X11 desktop (ADR 0025 §8) takes the Deck's layout only
  when the shared `console_session::guest_layout::guest_layout_policy` says
  `ApplyOnAttach` (its client proved absent just before, `platform.session.guest_layout`
  not `keep`); an attached or unknown client is `Shown`. Lay it out only in
  modes of the Deck monitors' own sizes with identity transforms (shared
  `plan_native_console_layout`; add a missing mode, never `ViewPortIn`
  scaling), and leave it when the Deck goes (`LayoutDeparture::LeaveDecks`:
  the hand-back and the recovery keep it; a failed or unproved layout is
  still put back and the desktop shown as it is).
- A console sign-in is decided by the shared `SwitchLedger::decide` before
  the launcher runs (ADR 0025 §13, `net/account_switch.rs`, facts in
  `session/handover.rs`). The account's own desktop is joined, except a
  desktop another product started while that product's own client uses it
  (`console::another_products_client`, refused
  `console_in_use_other_product` at sign-in, on re-attach and during an
  attachment, as before 0.21.5). Another account's session is
  signed out only after a confirmed offer, by
  `org.gnome.SessionManager.Logout` mode 1 asked as that account on its own
  bus, after `IsInhibited(1)` found no inhibitor; never mode 2,
  `loginctl terminate-session`, a signal or a kill. Only logind's read-back
  that exactly that session (id and `TimestampMonotonic`) is gone lets the
  requester in. Advance the registry's attachment epoch on every attach and
  detach, and never attach anyone the ledger holds out.
- Whoever connects gets their own layout (ADR 0025 §6): which layout a
  console is asked to take is the shared `connect_layout::console_layout_ask`
  (a one-display Deck that matches its layout too, on another product's
  desktop). A sign-in joins a console afresh (the takeover hands a held
  one back); never reattach a held console, or lay a journalled layout,
  for another Deck.
- Remote seated sessions get polkit's `allow_any` answers only through
  Arcen's rule `05-arcen-remote-session.rules` (shared
  `arcen_session::polkit_remote`, adapter `session/polkit_remote.rs`) — keep
  `set_rhost` and `set_tty("arcen")` on every launcher path. A seated
  desktop without the rule is refused before launch; every desktop is
  proved with `pkcheck` before hand-over and refused
  (`LauncherError::PolkitUnprotected`) on a grant (a seated one also when
  polkit does not answer). A native Wayland desktop is seated
  (`DesktopSeat::ArcenSeat`, `polkit_remote::desktop_seat(provider,
  native_seat)`), never seatless; its seat is not built unless
  `native_seat::polkit_blocker` passes. Never add a second polkit rule or
  module for one product path. Site restrictions must sort before `05-`.
