# Linux Wayland output inventory boundary

**Status:** the headless GNOME/Mutter session adapter exists behind the
default-off `wayland-provider` Cargo feature. A Pier built with it chooses
its provider from host facts (see [Provider selection](#provider-selection)):
an installed Xorg server keeps the dedicated X11 session; a host without
Xorg whose headless compositor has a DRM render node and a valid
qualification record (`arcen-pier wayland qualify --write`, see
[Host qualification](#host-qualification-wlp-wiring)) serves the headless
Wayland session to a real Deck end to end: PAM, the session at the Deck's
display size, capenc's Wayland Auto composition (see
[Wayland Auto and Speed](#wayland-auto-and-speed-wlp-auto-speed)), the
session-bound input injector (see [Input](#input)), the session's own
audio graph (native PipeWire and PulseAudio applications) and the Deck
microphone (see [Audio](#audio-wp-audio)), reattach and teardown (see
[Serving a Deck](#serving-a-deck-wlp-wiring)). It is not a released
provider yet. libei is spoken only to probe the compositor's EIS seat for an
emulated stylus (see [Pen and Hard USB](#pen-and-hard-usb-in-a-wayland-session));
pointer and keyboard stay on Mutter `RemoteDesktop` D-Bus.

A build without `wayland-provider` refuses `capture.provider = "wayland"`
at configuration load and, on a host without Xorg, refuses to start.
The output-inventory and libei seams described under
[Public host-local API](#public-host-local-api) are separate and remain
detection-only. capenc's default-off `wayland` feature adds a `PipeWire`
capture consumer, import routes (see "Capture leases and GPU import" below)
and the Auto/Speed compositions; `wayland-provider` turns it on in the fused
Pier, so such a build links `libpipewire-0.3`.

Qualification evidence for unmodified compositor stacks is kept in
`wayland-qualification/`. The first entry,
[Rocky Linux 9 / GNOME 40 / NVIDIA GRID](wayland-qualification/rocky9-gnome40-nvidia-grid.md),
is a baseline: eight-bit shared-memory
capture and Mutter D-Bus input only, with no DMA-BUF, damage metadata, EIS,
pen, ten-bit or HDR. The primary target,
[Rocky Linux 10 / GNOME 49 / NVIDIA GRID](wayland-qualification/rocky10-gnome49-nvidia-grid-nodrm.md),
was measured without DRM, with `nvidia-drm modeset=N` and with
`modeset=1`: Auto is a qualified candidate on all three; Speed reaches
60 fps only with a 120 Hz virtual monitor and an offer above 60, which a
Speed session now gets when the host record qualifies it (the product
qualification measured 50–54 fps of pointer motion on that host, so Speed
there is still served as Auto); the `gpu-convert` DMA-BUF import is proven
there ([GPU import and the paced Speed output](#gpu-import-and-the-paced-speed-output-wlp-gpu-speed)); Grading and HDR are unavailable (eight-bit
formats only, virtual monitors have only the default colour mode: Mutter
49 has no HDR screen sharing, which arrived in Mutter 50 with mutter!4928;
see [Wayland Grading and HDR](#wayland-grading-and-hdr-wp-fidelity)); EIS
and damage metadata exist; SELinux-enforcing hosts need Arcen's policy
module ([Linux Pier under SELinux](../security/linux-selinux.md)).

## Headless Mutter session adapter (`wlp-session`)

Code: `hosts/linux/src/session/wayland/` (Linux and `wayland-provider`
only, except the pure message and input types in `mod.rs`), the Wayland
branch of `hosts/linux/src/session/launcher.rs`, and the pure policy in
`arcen_session::wayland_session`.

### Process and session model

```text
Pier (root) ─ session-launcher (root; PAM `login`, pam_systemd)
              │   own logind session: Type=wayland Class=user Seat=<none>
              └─ session-agent --wayland-headless (session user, same scope)
                   ├─ dbus-daemon --session        private bus
                   │    └─ org.gnome.SessionManager   served by the agent
                   ├─ gnome-keyring-daemon --unlock    (optional)
                   ├─ pipewire, wireplumber        video and the session's audio sink; no hardware
                   ├─ pipewire -c pipewire-pulse.conf   the session's PulseAudio-protocol server (optional)
                   ├─ at-spi-bus-launcher, at-spi2-registryd   accessibility (optional)
                   ├─ gnome-shell | mutter --headless --wayland [--no-x11]
                   │    --wayland-display wayland-0 --virtual-monitor WxH@R
                   │    └─ Xwayland :n (Mutter's own; see X11 applications)
                   └─ gsd-* and XDG autostart programs  (optional)
```

- **One logind session per desktop.** The launcher opens a fresh PAM
  session with `XDG_SESSION_TYPE=wayland`, class `user`, no seat and no VT,
  and verifies with `loginctl` that it got exactly that and that it is the
  session leader. GNOME Shell 40 attaches to and registers whatever logind
  session started it (baseline finding 5); started from this launcher, that
  is the desktop's own session, never an operator shell or the Pier
  service. It still asks GDM to register the session; GDM ignores a session
  it does not own (observed on the baseline host, nothing logged).
- **Private runtime directory** `/run/user/<uid>/arcen-wayland-<session>`
  (mode 0700, created by the agent, must not pre-exist). It holds the
  private bus `bus`, `pipewire-0`, `wayland-0` and the input socket. It lives
  under logind's runtime directory, so logind removes it with the user's
  last session even if Arcen could not. A second desktop of the same user
  therefore never shares a bus name, `PipeWire` daemon or Wayland socket.
- **Private D-Bus session bus** (`dbus-daemon --session`). Mutter's
  `ScreenCast`, `RemoteDesktop` and `DisplayConfig` names and every service
  GNOME Shell activates live on it, not on the user's systemd bus. The agent
  does not call `dbus-update-activation-environment`.
- **Accessibility bus started by the agent.** Before the compositor, the
  agent starts `at-spi-bus-launcher --launch-immediately` and, once it owns
  `org.a11y.Bus`, `at-spi2-registryd`. Activated by the private bus
  instead, both are refused under SELinux (`unconfined_dbusd_t` would
  enter `unconfined_r:gnome_atspi_t`, an invalid context; see
  [the SELinux note](../security/linux-selinux.md#the-accessibility-bus-in-a-headless-wayland-session)),
  and every application logs "AT-SPI: Error retrieving accessibility bus
  address". They are optional members: a missing binary, a launcher that
  does not take the name within 5 s, or one that exits later is logged and
  the desktop runs without accessibility; none of these ends the session.
  Teardown stops them before the private bus.
- **PipeWire never opens hardware.** `WirePlumber` runs with a private
  configuration: the distribution's and administrator's files linked one by
  one, plus an override that disables the ALSA, BlueZ, V4L2 and libcamera
  monitors (`51-arcen-video-only.lua` for 0.4, a `wireplumber.conf.d`
  fragment for 0.5). A private session manager on a private bus cannot see
  PulseAudio's device reservations, so it must never open audio hardware.
  Both are proven: 0.4 on the GNOME 40 baseline, 0.5.10 on Rocky Linux 10
  (the fragment keeps every ALSA, BlueZ, V4L2 and libcamera monitor
  unloaded). Its state (`XDG_STATE_HOME`) is the private runtime directory,
  never the user's `~/.local/state/wireplumber`.
- **Audio is the session's own graph** (see [Audio](#audio-wp-audio)): a
  session sink in the private `PipeWire` and a private PulseAudio-protocol
  server, so native PipeWire and PulseAudio applications are both heard, and
  the Deck microphone is visible to both. A host whose audio server is
  PulseAudio itself bridges the private graph into it instead.
- **`GVFS_DISABLE_FUSE=1`** for the stack, so gvfs never mounts inside the
  private runtime directory. The document portal still mounts `doc/` there;
  teardown unmounts it.
- **Locale.** PAM's `login` stack sets none, and no display manager is
  involved, so the launcher gives the desktop the locale a display manager
  would (shared `arcen_session::session_locale`, read by
  `session::locale`): the user's AccountsService language
  (`Languages=`/`Language=`) as `LANG`, otherwise the system's
  `/etc/locale.conf` `LANG` and `LC_*` (never `LC_ALL`), plus the user's
  `FormatsLocale` for the regional categories; a value PAM supplies is kept.
  The same applies to the dedicated Xorg desktop, whose agent also passes the
  locale to its bus and systemd user manager. Without it the whole desktop
  ran in the `C` (ASCII) locale. Rocky Linux 10 lab: gnome-shell and the
  private bus's services carried `LANG=en_GB.UTF-8` from the system, and
  `LANG=nb_NO.UTF-8` after `SetLanguage nb_NO.UTF-8` through AccountsService;
  Rocky Linux 9 dedicated Xorg: gnome-shell and the systemd user manager
  carried the system's `LANG=en_US.UTF-8`, beside `TZ`.
- **Capacity one is unchanged.** The admission gate, grants and reconnect
  ownership are the existing ones; the launcher creates one desktop.

### GNOME login services (`wp-gnomesession`)

A GDM login runs gnome-session, which starts the settings daemons and the
XDG autostart programs, serves `org.gnome.SessionManager` (presence, idle,
inhibitors, logout) and leaves the login keyring unlocked. Since GNOME 49,
gnome-session can only run its session through the user's systemd manager
([GNOME 49 release notes](https://release.gnome.org/49/developers/): "The
service manager in gnome-session has been removed, with systemd being used in
its place"). That puts every service outside the desktop's logind scope,
private bus and private runtime directory. It also refuses a second session
of a user who is signed in elsewhere. And in a seatless session (always
`Active` to logind) it would report `SessionIsActive=true`, so `gsd-power`
would suspend a bare-metal host after its idle timeout (EL10 default
`sleep-inactive-ac-type='suspend'` after 900 s). GDM's remote login
(`gdm-new-session --headless`, GNOME 46+) hands session creation and PAM to
GDM. So the agent assembles the same login itself, with the decisions in
`arcen_session::desktop_session` and the Linux glue in
`session::wayland::{session_services, session_manager, keyring, autostart}`:

- **Session manager.** Before the compositor starts, the agent claims
  `org.gnome.SessionManager` on the private bus. It serves what GNOME Shell
  and the settings daemons use:
  - presence, driven by Mutter's `IdleMonitor` and the user's
    `org.gnome.desktop.session idle-delay`, and, as gnome-session's does,
    by the shell's screen shield (`org.gnome.ScreenSaver.ActiveChanged`):
    a shield that comes up is idle, and one the Pier lifts on a
    password-authenticated attachment makes presence available again with
    the next idle period starting at the user's next activity (an idle
    watch armed earlier would fire at once, Mutter counting from the last
    input, and lock the desktop just unlocked);
  - inhibitors (`org.freedesktop.ScreenSaver` reaches them through
    `gsd-screensaver-proxy`); an idle inhibitor keeps presence from going
    idle;
  - client registration (`DESKTOP_AUTOSTART_ID`);
  - `Setenv` (Mutter's `DISPLAY` when Xwayland starts);
  - `Logout`.

  On purpose it reports `SessionIsActive=false` and `CanShutdown=false`, and
  never sets logind's `IdleHint`. The first is the upstream gate that keeps
  `gsd-power` from dimming, blanking or suspending
  ([gsd 47 `idle_configure`](https://gitlab.gnome.org/GNOME/gnome-settings-daemon/-/raw/47.2/plugins/power/gsd-power-manager.c)),
  and that keeps the shell from automounting the host's media. The second
  hides Power Off and Restart.
- **Logout.** The shell's Log Out opens its own end-session dialog. On
  confirmation the session manager sends `EndSession` to its clients
  (bounded), and the agent ends the desktop through the orderly teardown
  below (logged `user_logout`).
- **Screen lock.** GNOME Shell creates its screen shield only when GDM
  answers on the system bus (`main.js`: `if (LoginManager.canLock())`),
  because it unlocks through GDM's reauthentication channel. With GDM the
  user's own settings decide the idle lock, lock delay and Super+L. Without
  GDM the shell cannot lock at all; the session also pins
  `org.gnome.desktop.lockdown disable-lock-screen` so nothing else tries.
  As on an Xorg desktop, the idle lock also comes while a Deck is attached
  but sends no input for `idle-delay` (default 300 s; the shield blanks the
  stream to black, and a key or click shows the unlock dialog), and every
  password-authenticated reattachment unlocks the desktop
  (`loginctl unlock-session`, logged `authenticated logind session
  unlocked`). A desktop that "came back locked" after a reconnect was this
  idle lock reached during an attachment without input, not a failed
  unlock.
- **Keyring.** `pam_gnome_keyring` would unlock a daemon on the user's systemd
  bus, not the private bus applications use. So the launcher hands the login
  password to the agent through its standard input (see
  [trust boundaries](../security/trust-boundaries.md#linux-desktop-session-services)).
  Before the compositor starts, the agent starts
  `gnome-keyring-daemon --foreground --unlock --components=<enabled>
  --control-directory <runtime>/keyring` and writes the password to it. The
  daemon owns `org.freedesktop.secrets` on the private bus and unlocks (or
  creates) the login keyring. The components are the union the enabled
  `gnome-keyring-*` autostart entries name, so a user who hid
  `gnome-keyring-ssh.desktop` gets no SSH agent. `GNOME_KEYRING_CONTROL` and
  `SSH_AUTH_SOCK` reach the shell and every application.
- **Settings daemons and autostart.** Once the Wayland socket exists, the
  agent reads `~/.config/autostart` and `$XDG_CONFIG_DIRS/autostart`, applies
  the autostart specification and GNOME's extensions (phases, conditions,
  delay, auto-restart within a budget), and starts what remains. These are
  optional members: none ends the desktop, and all stop before the
  compositor at teardown. Entries that act on the host are never started:
  the time-zone, radio, sharing, USBGuard and modem plugins, and
  virtual-machine console agents. `gsd-xsettings` waits until Mutter
  announces an X11 display. The keyring and the accessibility bus are
  started by Arcen itself, earlier.
- **Settings overlay.** A per-session dconf database
  (`<runtime>/dconf/arcen.db`, listed by `DCONF_PROFILE` right after the
  user's own database) pins `sleep-inactive-ac-type`,
  `sleep-inactive-battery-type` and `power-button-action` to `'nothing'`
  (and `disable-lock-screen` without GDM). Settings shows them as locked. The
  user's own settings are untouched and apply again in any other session.
- **Login-shell environment.** As a GDM login does, the agent runs the
  user's login shell once (bounded) and adds what `~/.profile` sets, such as
  `PATH` or a licence server. Variables Arcen owns are filtered out. The
  dedicated Xorg desktop does the same.
- **First login.** By default GNOME's initial-setup assistant
  (`gnome-initial-setup --existing-user`, an autostart entry conditioned on
  `~/.config/gnome-initial-setup-done`) and the shell's welcome tour run as
  after a GDM login. `wayland.first_login_setup = "skip"` writes GNOME's own
  completion markers for the user instead.
- **Host sleep.** For every desktop, on either provider, the launcher holds a
  logind `sleep:idle` block inhibitor (`systemd-inhibit --who=Arcen`). This
  also covers the GDM greeter's own `gsd-power` on the console seat.
- **Dedicated Xorg desktop.** It shares the login-shell environment and the
  host-sleep inhibitor. Rocky Linux 9.7 / GNOME 40 lab, 2026-10-18: during
  a desktop, logind listed `Arcen … sleep:idle … block`, and nothing after
  teardown. gnome-shell carried `PATH` and a licence variable from
  `~/.bash_profile` but not the `LD_PRELOAD` set there. The rest of this
  section is Wayland-only. On X11 the desktop shares the user's systemd bus
  with any console session of the same user, so a session manager or
  settings daemons there need their own design.
- **Not fatal.** Nothing here is a readiness step. A missing binary, a name
  already taken, a failed compile or an exec SELinux refuses is logged, and
  the desktop runs without that part.

### GPU access

A headless session is seatless, so logind grants it no device ACLs: no DRM `card`
node and no input nodes. (A native desktop's session is on its own seat
and gets its seat's devices from logind; see
[Native desktops](#native-desktops-on-their-own-seat-waylandsession_mode).) Arcen does not change `/dev/dri` permissions, add
users to `video` or `render`, or touch SELinux, Secure Boot or kernel
parameters. On the GNOME 40 host Mutter therefore logs
`Failed to open gpu '/dev/dri/card0': Permission denied` and runs its
surfaceless renderer, which the baseline shows still composites on the
NVIDIA GPU through EGL; `card0` would not help there anyway (`nvidia_drm
modeset=N`, no connectors), and it is the GPU the live Xorg sessions drive.
The consequence is shared-memory `BGRx` capture only (Mutter disables
DMA-BUF when surfaceless). A DMA-BUF path needs a compositor that renders
headless on the render node (Mutter 50 does, GNOME 40 does not) and
`nvidia-drm modeset=1`; that is the newer-GNOME qualification host, not a
permission change. The render node itself (`crw-rw-rw-` on the baseline
host) is reported as a separate fact. On Rocky Linux 10 (Mutter 49) the
same holds per driver state: with no DRM device Mutter composites on
NVIDIA through EGL surfaceless; with `nvidia-drm modeset=N` it adopts the
render node through Mesa GBM and renders with llvmpipe; with `modeset=1`
it creates a GBM renderer on the NVIDIA render node and offers DMA-BUF
with block-linear modifiers (see the Rocky 10 qualification).

### Outputs (`MutterVirtualOutputProvider`)

`arcen_outputs::OutputProvider` with `OutputSurface::Virtual`,
`ExactRestore`, exact modes, persistent and headless, 1 region:

| Stage | What happens |
| --- | --- |
| preflight | compositor binary present, runtime directory absent; nothing mutated |
| bind | start the stack with one `--virtual-monitor WxH@R` per output; the stack's `Drop` kills every member and sweeps the scope if the bind is cancelled |
| verify | `DisplayConfig.GetCurrentState` on the private bus; the shared `verify_virtual_outputs` requires exactly the requested monitors, each `MetaVirtualMonitor` with the requested current mode |
| commit | log the committed outputs |
| rollback | stop the stack, which removes its monitors; prove no process of the user remains in the scope and the runtime directory is gone |

The output geometry is the served Deck display (`SessionDisplayMsg`), or
1920×1080@60 when none was named.

Two output modes exist, chosen from the compositor's own `--version`
(`arcen_session::wayland_outputs::VirtualOutputMode`, never a distribution
name):

| Mode | Compositor | Outputs | Change during a session |
| --- | --- | --- | --- |
| startup monitors | Mutter/GNOME Shell below 49 | `--virtual-monitor WxH@R`, product `MetaVirtualMonitor`, one output | none |
| remote monitors | Mutter/GNOME Shell 49 and later (qualified on 49.4) | `ScreenCast.RecordVirtual` streams (`is-platform`), product `Virtual remote monitor`, up to four | resize in place, re-placement, topology change |

In the remote-monitor mode the compositor starts with no monitor. Binding
also creates the paired `RemoteDesktop`/`ScreenCast` session with one
virtual stream per output and negotiates each stream once at the output's
exact size and refresh (`arcen_capenc::materialise_virtual_output`, a
PipeWire negotiation that reads no pixel): Mutter creates the monitor at
the negotiated size and maximum frame rate, keeps it when the consumer
leaves, and resizes it in place when a later consumer negotiates another
size. The agent names each output after the monitor that appeared, places
them (`DisplayConfig.ApplyMonitorsConfig`, temporary method, at the scale below;
several outputs named without positions sit left to right), and verifies
geometry and placement (`verify_virtual_outputs`, `verify_placement`).
Each output also carries the client display's density (the millimetres
the client advertised, and its logical width): a virtual monitor has no
physical size, so Mutter would leave it at UI scale 1, and the agent instead
places it at the scale `arcen_outputs::choose_output_scale` picks among the
scales its mode supports, and verifies it. The millimetres are the Deck's
Remote UI scale request (`pixels * 25.4 / (96 * percent / 100)`; a
backing-pixel stream doubles the percentage), read back with the rule the
Windows Pier applies — 96 DPI is 100% — and rounded to the nearest
supported scale, halfway taking the smaller; without millimetres a
backing-pixel stream takes its pixel-to-point ratio. Positions follow
Mutter's `layout-mode` (`GetCurrentState`): in the default physical mode
they are the desktop pixels and only whole scales are taken; in the logical
mode (`scale-monitor-framebuffer`, which also offers fractional scales) the
desktop layout is converted with `arcen_session::wayland_outputs::
logical_positions` (an output spans its size over its scale; shared edges
and their alignment stay), and scales that leave no logical layout fall
back to 1. A scale changes the UI, not which outputs touch.
Absolute pointer and touch coordinates arrive in output pixels and are
converted per stream by the shared `absolute_stream_coordinate`: Mutter
49–51 divide a `RecordMonitor` stream's coordinate by the monitor's scale
in the logical mode (`meta_screen_cast_monitor_stream_transform_position`,
51: `meta_stream_monitor_transform_position`) but add a `RecordVirtual`
stream's to its view layout undivided
(`meta_screen_cast_virtual_stream_transform_position`, 51:
`meta_stream_virtual_transform_position`), so a virtual stream in the
logical mode takes output pixels over the scale. The agent follows
`MonitorsChanged` for the layout mode and scales (and reads them again
after each placement). Mutter closes a monitor stream whose logical
rectangle changes; an input monitor stream that answers "Unknown stream"
is recorded again for the monitor as it is, or dropped so input returns
to the output's own stream.
Mutter returns to its own layout after any monitor change (a capture
epoch negotiating another refresh, a resize), so a placement keeper
re-applies the desired layout on every `MonitorsChanged`. Because a
virtual monitor takes its consumer's size, capenc offers exactly the
hand-off's size. The stream's refresh follows the consumer's negotiated
maximum frame rate (an Auto epoch runs the monitor at 30 Hz).

#### Output reconfiguration (`wlp-recovery`)

A remote-monitor agent serves `$XDG_RUNTIME_DIR/arcen-output.sock` (mode
0600; root or the session user; the Pier checks the peer is the agent):
`hello` reports the live outputs, streams and output generation;
`configure {generation, outputs}` moves the session to exactly those
outputs as one shared `OutputTransaction`:

| Stage | Same count (resize, move) | Another count (topology change) |
| --- | --- | --- |
| preflight | exactly the next generation; a valid layout (adjacent, non-overlapping, anchored at the origin, at most four) | same |
| bind | renegotiate each resized stream; place every output | on the running session: added outputs are new `RecordVirtual` streams started with `ScreenCast.Stream.Start` (a stream recorded after the session started announces its node only then; Mutter 41+), dropped outputs' streams are stopped, kept outputs keep their streams and are resized in place; if adding fails, a new paired session records the new outputs instead and the live ones stay |
| verify | `DisplayConfig` geometry and placement | same, for the new set |
| commit | — | the outputs are live (a replacement set: the old session stops and its monitors vanish) |
| rollback | renegotiate and place the previous outputs | stop the added streams (or record the dropped ones again), restore and place the previous outputs; a replacement set is stopped |

Held input is released first, and input arriving during the transaction
is refused at once (never queued into the next generation); on commit the
input facts move to the new generation and streams, so a coordinate of an
older generation is refused. `hello` answers from a snapshot of the last
committed outputs, so it never waits behind a running transaction; a
second `configure` while one runs is refused; up to four connections are
served at once and an idle one is closed after 30 s. A refusal carries the
outputs that stayed live, and the Pier captures exactly those; a session
whose outputs cannot be read is never assumed to have its start facts. The Pier stops every capture
epoch of the session first: a stream is renegotiated only while nothing
else consumes it. A refused or failed transaction leaves the previous
outputs and generation. Mutter closing the paired session while the
compositor runs makes the agent record the same outputs again (new
PipeWire nodes, next generation); the Pier's next capture epoch finds them
through `hello`.

### ScreenCast and RemoteDesktop ownership

The agent owns both D-Bus sessions on the private bus through the pure-Rust
`zbus` client: `RemoteDesktop.CreateSession`, then `ScreenCast.CreateSession`
with `remote-desktop-session-id` (so absolute pointer and touch address the
stream), `RecordMonitor(connector, {cursor-mode: …})` (the desktop's
cursor mode, embedded or metadata, see [Cursor](#cursor)), and
`RemoteDesktop.Session.Start`, which starts both. It waits for
`PipeWireStreamAdded(node)` and reads the stream's `Parameters` (size,
position). Mutter closes both sessions when the agent's connection goes
away; the agent keeps the connection for the whole session and calls
`Stop` before releasing the stack.

### Readiness

`arcen_session::wayland_session::WaylandSessionReadiness` is ordered:
compositor started → outputs verified → one stream per output at the
output's size → a started, linked `RemoteDesktop` target that accepted an
absolute pointer motion and offers keyboard and pointer → a real frame from
one of those streams at the stream's size. A running compositor, a socket or
a node id is never readiness. The agent reports steps 1–4 to the launcher
(the ready message carries outputs, streams, device types, input socket and
generation; the launcher checks it is consistent with what it asked for).
Step 5 belongs to the capture consumer: capenc states the first frame
(node, format, size, memory) and the route that encoded it in its
`ADMISSION` line before `READY`, and the Pier must not announce the desktop
to a Deck before it (`CapencSession::capture_admission`). Mutter sends one
frame when a consumer connects, even to a still desktop, so this needs no
damage. The lab harness proves it with `--pipeline auto|speed` (the product
capenc through `media::capenc::spawn`), or with `gst-launch-1.0 pipewiresrc`
without it.

Facts reported for `WaylandCapabilityFacts` on the GNOME 40 host: compositor,
session service, PipeWire and input injection available (proven by a
session); render-node access available by device mode; the capture route
proven per session by a first frame that capenc encoded through a reported
route (`FrameEvidence::import`, `host-copy` on GNOME 40); GPU import
unproven, because a host copy is not one (only `gpu-convert` or
`zero-copy-import` prove `gpu_import`, which Auto/Speed do not require).
Session proof never changes the Pier's start-time selection, which reads
host facts only: the capture route and input injection stay unknown there
until an installation-time qualification records them. Input capabilities from
`SupportedDeviceTypes = 7`: keyboard, pointer (with smooth and discrete
scroll) and touch available; pen and EIS missing.

### Capture hand-off to capenc

`arcen_media::video::capture_handoff::PipeWireCaptureHandoff` is the one
definition of the names and the one parser: the session agent builds it,
the launcher puts it into the session environment, which capenc inherits
through `UserExecution`, and capenc's `wayland/handoff.rs` only reads it
back with the same parser. This table is the reference for both sides.

| Variable | Required | Example | Meaning |
| --- | --- | --- | --- |
| `ARCEN_CAPTURE_PROVIDER` | yes | `wayland` | absent for Xorg |
| `XDG_RUNTIME_DIR` | yes | `/run/user/1000/arcen-wayland-301` | private runtime; its `pipewire-0` is the session's PipeWire |
| `PIPEWIRE_REMOTE` | no | | PipeWire's own override (socket name or absolute path); honoured, never set by the session |
| `ARCEN_PIPEWIRE_NODE` | yes | `36` | node from `PipeWireStreamAdded`; 0 and `u32::MAX` are refused |
| `ARCEN_CAPTURE_OUTPUT` | yes | `Meta-0` | connector the stream records |
| `ARCEN_CAPTURE_OUTPUT_GENERATION` | yes | `1` | the node is valid for this generation only |
| `ARCEN_CAPTURE_EXPECTED_SIZE` | yes | `1920x1080` | stream `Parameters` size |
| `ARCEN_CAPTURE_OUTPUT_POSITION` | yes | `0,0` | output logical origin |
| `ARCEN_CAPTURE_CURSOR_MODE` | yes | `embedded` | the desktop's `cursor-mode` (`embedded` or `metadata`); decides cursor authority |
| `ARCEN_CAPTURE_GPU_PCI` | no | `0000:00:10.0` | encoder GPU (same variable as the Xorg paths); set only when the compositor's proven GPU is NVIDIA (sysfs vendor `0x10de`) and the operator set no `ARCEN_CUDA_DEVICE` |
| `ARCEN_COMPOSITOR_GPU_PCI` | no | `0000:00:10.0` | PCI device of the compositor's render node |
| `ARCEN_COMPOSITOR_RENDER_NODE` | no | `/dev/dri/renderD128` | the one render node the compositor holds open |

The session environment also carries `DBUS_SESSION_BUS_ADDRESS` (the
private bus) and `WAYLAND_DISPLAY=wayland-0`; capenc uses neither. The
compositor GPU facts are evidence, not inference: the agent reads the
compositor's own `/proc/<pid>/fd` and names a render node only when it
holds exactly one, then resolves its PCI device and vendor through sysfs.
When that proof is absent no GPU variable is set and capenc's same-GPU proof
stays unknown.

The encoder is pinned to the compositor's GPU only when that GPU is an
NVIDIA device, the only kind capenc's native encoder opens by PCI address.
A compositor rendering on an integrated or emulated adapter next to the
NVIDIA GPU (an Intel or AMD iGPU, a hypervisor's `1234:1111` VGA) names no
encoder: capenc keeps its own device choice (`ARCEN_CUDA_DEVICE`, otherwise
CUDA device 0) and the compositor facts tell it the source is on another
GPU, so it takes the cross-GPU host-copy route instead of failing to open
CUDA and falling back to software. An operator's `ARCEN_CUDA_DEVICE` in the
session environment always wins over the pin.

No file descriptor crosses the boundary; capenc connects to PipeWire itself.
Pixel format, memory type and modifiers are not part of the hand-off,
because Mutter's D-Bus API does not state them: the consumer negotiates and
reports them as capture-surface evidence. On GNOME 40 that is `BGRx` in
shared memory only; when a stream records cursor metadata, it needs a
buffer of at least `CURSOR_META_SIZE(384, 384)` and cursor-only updates
arrive as size-0 buffers (baseline findings 3–4). The variables cannot be injected through
PAM: the launcher drops them from the PAM environment. The stream lives as
long as the agent's D-Bus connection; teardown ends it.

### Input

The agent serves `$XDG_RUNTIME_DIR/arcen-input.sock` (mode 0600 in the
0700 directory; peer uid must be root or the session user): newline JSON
commands (`hello`, `pointer_absolute`, `pointer_relative`, `pointer_button`,
`pointer_axis`, `pointer_axis_discrete`, `key`, `keysym`, `touch_down`,
`touch_motion`, `touch_up`, `release_all`, `locks`, `text`, `keymap`), each wrapped with a
deadline (`{"not_after_ns": …, "command": {…}}`), with one JSON reply each.
The deadline is the sender's `CLOCK_MONOTONIC` at sending plus how long it
waits for the reply (the Pier and the agent share the host's clock); a
command read after it is refused (`expired before it was read`) and never
injected, so a command written just before the session stalled, which the
Pier has since given up on, cannot arrive seconds late. A line without a
deadline is malformed.
Coordinates are stream pixels of a named output and carry the output
generation they were mapped for; an unknown output, another generation, or
a point outside the stream is refused. `hello` injects nothing and returns
the session's input facts: generation, streams, `SupportedDeviceTypes`,
whether the `RemoteDesktop` session started, the backend
(`remote_desktop_dbus`), whether the compositor offers `ConnectToEIS`
(introspected; GNOME 40 does not), and the connection number.

One connection is served at a time, and a newer one supersedes it: what the
old connection held is released first and the old connection is closed, so
an attachment that was replaced can never inject again. A waiting connection
is accepted before the current connection's next line is read, so a Pier
that reconnected after a stall supersedes its old connection before that
connection's leftover lines are read. A press is recorded
as held before it is sent and a release only after Mutter accepted it, so an
interrupted press is still released. Keys, buttons and touches still held
when a connection ends or is superseded, or the session ends, are released.
`RemoteDesktop` has no tablet device: a `pen` command (stream pixels,
generation, tool, proximity, contact, buttons, pressure, tilt, rotation) is
injected only through an emulated stylus on the compositor's EIS seat, and
`pen_away` takes it out of proximity; a pen in proximity is held like a key
and released the same way. See
[Pen and Hard USB](#pen-and-hard-usb-in-a-wayland-session).

Injection goes through `session::wayland::input::CompositorInput`, which
`MutterSessions` implements with `RemoteDesktop` `Notify*` calls. That trait
is the EIS seam: a libei injector built on
`RemoteDesktop.Session.ConnectToEIS` (GNOME 45 and later; Rocky 10 ships
GNOME 49 with libei 1.5) would implement it inside the agent, and the
socket protocol, validation, supersession and held-input release stay as
they are. Pointer and keyboard are not moved to EIS. On Rocky
Linux 10 / Mutter 49, `ConnectToEIS` on a session linked to the stream
offers a seat with pointer, absolute pointer, keyboard, touch, scroll and
button, and absolute devices carry the stream's region and mapping id. The
agent uses `ConnectToEIS` once at start, for the stylus probe below.

#### Pen and Hard USB in a Wayland session

**Tablet support (typed pen).** On Xorg the Pier creates a uinput
tablet-tool device. In a headless Wayland session that device would reach
the machine's physical seat, not the session, so the only route is an
*emulated stylus*: libei's `ei_stylus` interface, offered by the
compositor's EIS server. At start the agent calls `ConnectToEIS` on its
`RemoteDesktop` session. It then performs the libei handshake with the
pure-Rust `reis` client (no libei library is linked), announcing `ei_stylus`
v1, and reads which interfaces the server negotiated and which capabilities
its seat offers. It binds nothing and disconnects. The result is the
`stylus` member of the input `hello` (`StylusFacts`). It holds either a
binding, which the Pier turns into per-axis pen truth with
`arcen_input::pen_emulation::PenCapabilityTruth`, or a reason with
evidence:

| Reason | Meaning |
| --- | --- |
| `no_emulated_input` | the compositor has no `ConnectToEIS` (GNOME 40) |
| `stylus_not_offered` | the EIS seat negotiated no `ei_stylus` (GNOME 45–51 today) |
| `client_lacks_stylus` | the compositor offers `ei_stylus`, but this Pier's EI client cannot bind it yet |
| `connection_failed` | `ConnectToEIS` or the handshake failed or timed out |
| `not_probed` | the session was started by a Pier that predates the probe |

Frames for an emulated stylus come from the shared
`arcen_input::pen_emulation::StylusEmulator`, which enforces the protocol's
frame rules: proximity-in and proximity-out never share a frame; the
proximity-in frame carries position and every bound axis; tip and erase
edges never share a frame; pressure is above zero while the tip is down;
buttons travel only when the device has a button interface. Upstream state
(2026-10-18):

- libei merged `ei_stylus` in libinput/libei!400 (2026-07-29) for 1.7.0,
  which is not yet tagged; the newest release is 1.6.0.
- No compositor's EIS server offers a stylus: Mutter main (51.1) has no
  `ei_stylus` code and no merge request for it, and KWin has none either.
- `reis` 0.7.1 tracks libei 1.6 and keeps its wire API private, so a
  compositor that starts offering a stylus is reported
  `client_lacks_stylus`, with the negotiated version, until `reis` ships
  bindings.

Measured on Rocky Linux 10.2 / Mutter 49.4 / libei 1.5, a live session's
hello carries `eis_offered=true`, `stylus.unavailable=stylus_not_offered`
and seat interfaces `[ei_button]` of those announced; a `pen` command is
refused. Against libei's own `eis-demo-server` (libei main `a9bf31da`, built
for the lab only), `arcen-pier wayland eis-probe --socket PATH` reports
`ei_stylus` v1 offered (`client_lacks_stylus`). A lab-only client built on a
locally regenerated `reis` delivered a `StylusEmulator` stroke through the
same server with no protocol error: proximity, tip, pressure, tilt,
rotation and eraser.

**Native tablet (Hard USB).** A bridged tablet is claimed by the host
kernel, and its input nodes join the machine's logind seat. Mutter in
headless mode runs with `META_SEAT_NATIVE_FLAG_NO_LIBINPUT`
(`src/backends/native/meta-backend-native.c`: "No seat assigned, running
headlessly") and opens no input devices. Bridging into a Wayland session
would therefore deliver the tablet to whatever runs on `seat0` instead
(unless the lab seat pen route below is used).
The shared `arcen_usb_bridge::admit_hard_usb_session` refuses Hard USB, and
quarantined raw HID, for every seatless (Wayland) session. The hello does
not offer `usb_hard_v1`, and a Native tablet request is answered with that
reason and a pointer to Tablet support.

Routes considered for current GNOME:

- **A dedicated logind seat for the session.** This is the product route
  on stock GNOME: see [Native desktops](#native-desktops-on-their-own-seat-waylandsession_mode)
  below.
- **Hard USB as an emulated stylus.** The Pier grabs the bridged device's
  evdev node (`EVIOCGRAB`, as libei's `ei-libinput-stylus` does) and
  forwards it as EIS stylus. This has the same compositor prerequisite as
  Tablet support, and the pad and vendor-driver settings would be lost.
- **Headless libinput on a private udev seat (lab, `wayland.lab_seat_pen`,
  default off).** This is the route that works today. It needs a compositor
  built with GNOME/mutter!5224 ("seat/native: Enable libinput in headless",
  open upstream) plus the Arcen lab edit
  (`packaging/linux/tools/gnome-prefix/patch-headless-seat-51.py`):
  - The compositor runs libinput only when `ARCEN_LAB_HEADLESS_INPUT_SEAT`
    names the udev seat.
  - It opens its devices directly, never through logind's `TakeDevice`,
    which a seatless session cannot use. !5224 still asks logind for
    devices that exist at start.
  - It opens them non-blocking and close-on-exec. !5224 opens with a plain
    `O_RDWR`, so libinput blocked in `read()` until the pen's first event:
    startup stalled and tool events never reached clients.

  With the flag on, every headless Wayland desktop gets its own pen
  (`input::seat_pen::SeatPen`, pure plan in shared
  `arcen_input::seat_pen::DesktopPen`). Before the desktop's compositor
  starts, the Pier:

  1. Draws a random desktop id and writes a volatile udev rule for it alone
     (`/run/udev/rules.d/72-arcen-lab-seat-pen-<id>.rules`). The rule matches
     only a virtual device (bus `0006` under `/devices/virtual/input/`,
     a2ce:0010) with that desktop's unique name ("Arcen Seat Pen <id>", a
     random 64-bit id), and puts it on `seat-arcen-pen-<id>`, never
     `seat0`. A physical device that copies the name has another bus and
     devpath and is not taken.
  2. Creates the tablet, a2ce:0010: pen and eraser tools, tip, two barrel
     buttons, pressure, tilt, and a 32768² raster at 100 units/mm.
  3. Proves udev put it on that seat, then clears every ACL entry on its
     event node and grants that desktop's user, and no one else, read and
     write (`setfacl -b`, then `setfacl -m u:<uid>:rw`).
  4. Tells that compositor, and only that one, its seat through the
     launcher, the agent (which accepts only `seat-arcen-pen-<id>`) and its
     own environment.

  Any failed step undoes the earlier ones, and the desktop gets no lab pen
  (pen stays on the EIS seat). The desktop owns its pen: an attachment
  routes pen samples only to the pen of the desktop it serves
  (`SessionLease::seat_pen`), and the hello advertises the uinput tablet's
  axes only then. When the desktop ends, the Pier revokes the grant,
  destroys the device and removes the rule. Pier start and exit sweep any
  seat-pen rule left behind; a crashed Pier's devices and their ACLs died
  with its process.

  The compositor's libinput sees a real tablet, so applications get
  `zwp_tablet_v2` (and Xwayland gets XI2 valuators), as with a USB tablet.
  Physical devices stay root:input and the session cannot open them: a
  run without the ACL logged "Permission denied".

  Shipping depends on upstream taking the three fixes above into !5224
  (draft comment prepared). Release/Security must also accept the ACL on
  the session's own virtual pen: `hosts/linux/AGENTS.md` forbids granting
  a compositor input access, and the intent of that rule is host devices,
  but this is still an input grant.

Lab evidence for the seat pen (Rocky Linux 10.2, NVIDIA GRID V100D,
2026-10-19):

- **A plain `zwp_tablet_v2` client.** It received `tablet_added`
  (`Arcen Lab Pen`, a2ce:00ff), `tool_added` (pen, capabilities tilt and
  pressure), proximity, tip, a pressure ramp (501 to 34887 of 65535),
  tilt (-21.3, 17.8) to (28.4, -7.1) and barrel buttons. This held on
  Mutter 51.1 from the side-by-side prefix and on the distribution's
  Mutter 49.4 with only `libmutter-17` rebuilt from Rocky's own SRPM with
  the same edit.
- **Stock Mutter.** It opens no input device.
- **The real Deck GUI.** It drove the dev Pier (prefix 51.1,
  `wayland.lab_seat_pen` on) with synthetic tablet events through the
  real-screen harness. Firefox in the session's GNOME Shell 51.1 received
  `pointerType: "pen"` for 987 moves and the stroke, with 33 distinct
  pressures (0.037–0.638). The Deck's glass showed the stroke widening
  with pressure.
  - Deck-side latency (over the Norway→London VPN link): input → host
    event about 23 ms p50, motion-to-photon about 123 ms p50.
  - Glass: 59.5 fps (Speed).
- **One pen per desktop.** The same run with per-desktop pens: Firefox got
  1033 pen moves with 35 pressures (0 to 0.638); motion-to-photon was
  108 ms p50.
  - The desktop's rule named only its own device and seat
    (`seat-arcen-pen-<id>`). udev reported that seat, the devpath under
    `/devices/virtual/input/` and `ID_INPUT_TABLET=1`.
  - `getfacl` on the event node showed one named user, the desktop's.
  - When the Pier stopped, the desktop's pen logged "revoked, destroyed and
    its rule removed". The node was gone, and no rule was left.
  - Two stale rules planted before a Pier start were swept at start
    (`removed: 2`).

#### Native desktops on their own seat (`wayland.session_mode`)

Code: pure policy in `arcen_session::native_seat` (mode, host facts,
qualification proof, the per-desktop plan and its reasons); the seat's udev
rule in `arcen_input::seat_pen` (`NativeSeat`, `native_udev_rule`,
`native_release_rule`, the `61-mutter.rules` filter); the host side in
`hosts/linux/src/input/seat_pen.rs` (`SeatPen::create_native`,
`native_seat_host_facts`), the launcher (`XDG_SEAT` in the PAM env, the
seat check), the agent (`--native-seat` → `--display-server`), the
qualification step (`qualify_native_seat`) and its Wayland client
(`session::wayland::tablet_proof`).

**Why.** Stock Mutter runs libinput only in native mode: headless mode sets
`META_SEAT_NATIVE_FLAG_NO_LIBINPUT` and drops the logind launcher
(`src/backends/native/meta-backend-native.c`). Native mode needs:

- a logind session on a seat (the launcher finds it by `XDG_SESSION_ID`
  and takes control);
- at least one KMS device with a connector on that seat
  (`meta-monitor-manager-native.c`: "No GPUs with outputs found").

A vGPU or data-centre GPU has no connectors, which is also why GDM's own
login screen fails on these hosts at every boot. So a native desktop's seat
(`seat-arcen-pen-<id>`) holds three devices:

- **The NVIDIA card node** (renders). Render nodes are not seat devices and
  stay shared with every headless desktop.
- **The in-tree `vkms` card**, the display head Mutter requires.
- **The desktop's virtual tablet.**

One volatile rule in `/run/udev/rules.d/` puts them there. The launcher
opens the PAM session with `XDG_SEAT` set to the seat, and logind activates
a session on a seat without VTs at once. The agent starts the compositor
with `--display-server` instead of `--headless`. logind hands it every
device through `TakeDevice`. Arcen's outputs are the same `RecordVirtual`
monitors, and the remote-monitor placement disables the `vkms` monitor.

**Modes.**

| `wayland.session_mode` | A new Wayland desktop |
| --- | --- |
| `auto` (default) | native when the qualification record proves it and the seat can be built now; otherwise headless, and the Pier logs `this Wayland desktop is headless (no real pen)` with the reason |
| `native_seat` | native, or the desktop is refused (`native_seat_unavailable`) |
| `headless` | headless, as before native desktops existed |

The fallback reasons are the stable tokens of `NativeSeatUnavailable`:

- `not_qualified`, `qualification_failed`;
- `no_gpu_card`, `gpu_has_connectors` (the GPU may drive a console),
  `gpu_in_use` (its card node is open, as for an Xorg server);
- `vkms_missing`, `vkms_in_use` (one native desktop per host);
- `polkit_no_rules` (polkit older than 0.106), `polkit_rule_missing`,
  `polkit_unverified` (polkit's tools did not answer),
  `polkit_grants_remote_session` (the proof after launch found a grant;
  see Security) — the shared `PolkitUnprotected` tokens;
- `greeter_on_seat`, `seat_setup_failed`.

**Building a seat** (`SeatPen::create_native`, one per host):

1. The live facts must have no blocker.
2. The rule and the `61-mutter.rules` override are installed.
   `mutter-common` tags `vkms` `mutter-device-ignore`. The tag is sticky
   (`TAG-=` clears only the current tags), so the override (the
   distribution's file without those two lines) lives in
   `/run/udev/rules.d/` while the seat does.
3. The NVIDIA card is retriggered, and logind's seat is awaited.
4. After a settle delay, `vkms` is loaded.
5. Every device's `ID_SEAT` is verified, and the tablet is created on the
   seat.
6. The seat must hold no session yet.

The order defends the seat against a display manager:

- GDM starts a login screen on every new graphical seat, on `SeatNew`,
  on a `CanGraphical` change and when it starts (`ListSeats`).
- With only the NVIDIA card on the seat that login screen fails at once.
- `vkms` joining later changes no `CanGraphical`, so no new login screen
  starts.
- A seat that already holds a session is refused (`greeter_on_seat`). The
  desktop then falls back to headless, or is refused under `native_seat`.

**Taking it apart.** When the desktop ends, or the Pier stops, the
launcher's shutdown does it on a blocking thread, after the desktop's
launcher and agent have stopped. A desktop that fails to open is taken
apart the same way, after its launcher stops, so its compositor has let go
of `vkms`.

1. The tablet is released and destroyed.
2. `vkms` leaves the seat by dropping its *current* `seat` and
   `master-of-seat` tags (logind reads current tags). The seat, and any
   login screen on it, goes with it, without `vkms` ever joining `seat0`.
3. `vkms` is unloaded, retried for up to 15 s while the compositor exits.
4. The rules and the override are removed.
5. The NVIDIA card is retriggered back to `seat0`.

The Pier's start and exit sweep catches whatever is left:

- a `vkms` that Arcen loaded (`/run/arcen/vkms-loaded-by-arcen`, written
  after `modprobe`), or one still on an Arcen desktop seat, is taken off
  every seat by tag and unloaded;
- the marker is removed only after a successful unload;
- this runs whether or not a rule file was left behind.

**Proof** (qualification, `native_seat` step and record section
`facts.native_seat`):

1. The qualification account gets one native desktop.
2. `tablet_proof` connects to its compositor as a plain Wayland client and
   binds `zwp_tablet_manager_v2`. It needs the desktop's tablet announced
   by its own name.
3. The tablet is brought into proximity, and the client needs the pen tool
   announced with pressure and tilt. Tool announcements need no surface or
   focus.

A host that cannot have a native seat records why, and its desktops stay
headless. A record without the section (every record older than this
build) means `auto` stays headless until the host is re-qualified.

**Security.**

- **polkit.** polkit counts a subject as local when its logind session has a
  seat (`is_session_local`, which ignores `Remote`). Without a rule, a
  native desktop's remote user would get every `allow_active` grant:
  power-off, reboot, suspend, network control, mounting, the clock.
  - Native desktops use the one mechanism every remote seated desktop
    uses (X11 desktops on `seat0` too): `arcen_session::polkit_remote`
    and the static rule `/etc/polkit-1/rules.d/05-arcen-remote-session.rules`
    (administration guide, "polkit"). It answers for sessions with
    `REMOTE=1`, `TTY=arcen` or a `seat-arcen-pen-*` seat with each
    installed action's own `allow_any` default, which is what a seatless
    headless session gets; an action it does not know gets `AUTH_ADMIN`.
    There is no native-seat rule of its own; a
    `49-arcen-native-seat.rules` left by a development build is removed.
  - The installer installs the rule (`arcen-pier polkit-rule install`)
    before qualification, the Pier rewrites it at every start, and
    `--uninstall` removes it.
  - **Classification.** A native desktop's logind session sits on its
    Arcen seat, so admission classifies it `DesktopSeat::ArcenSeat` (a
    headless desktop is `Seatless`, an X11 desktop `ConsoleSeat`). The
    proof after launch judges on the stricter of the planned seat and the
    seat logind recorded for the session, so a seated session is never
    judged seatless.
  - **Before the seat is built** the launcher reads polkit's state
    (`native_seat::polkit_blocker`): without the rule
    (`polkit_rule_missing`), with polkit older than 0.106 (no JavaScript
    rules, as on Ubuntu 22.04: `polkit_no_rules`) or with tools that do
    not answer (`polkit_unverified`), the desktop is headless (`auto`) or
    refused (`native_seat`) before `vkms` is loaded.
  - **Before handing it over**, the shared proof asks polkit about the
    session's agent with `pkcheck --process pid,start,uid` for the
    installed probe actions: `login1` power-off, reboot and suspend,
    NetworkManager `network-control`, udisks2 `filesystem-mount`, timedate1
    `set-time`.
    - Every answer must be "not authorized" or "needs admin".
    - Otherwise the desktop's processes are killed, the desktop is refused
      (`polkit_grants_remote_session` or `polkit_unverified`; the Deck is
      told the host refused it for polkit), and until that Pier restarts
      its later desktops are headless (`auto`) or refused (`native_seat`).
    - Qualification asks again by the same path and records
      `polkit_effective` only for an admission; a proof without it proves
      nothing.
- The tablet's event node gets no ACL. Only logind opens it, for the
  session controller of the session on that seat. logind refuses
  `TakeDevice` for devices of another seat and for seatless sessions, and
  it revokes evdev fds with `EVIOCREVOKE` when the session goes inactive or
  ends.
- The card nodes carry logind's usual `uaccess` ACL for the desktop's user
  only while its session is active on that seat.
- A `closing` session (processes left behind) keeps its devices until its
  scope stops; the Pier's teardown stops it.
- A display-manager restart while a native desktop runs would put a login
  screen on the desktop's seat. That screen becomes the active session and
  freezes the desktop. The installer explains this and offers
  `--disable-gdm` (recorded, undone by `--restore-gdm` and `--uninstall`).

**Supported and verified.** Every requirement is a host fact read at
qualification and before each desktop. A host that lacks one keeps its
desktops headless and records the reason.

| Requirement | Verified | Elsewhere |
| --- | --- | --- |
| GNOME Shell / Mutter running natively with libinput | Rocky Linux 10.2, GNOME 49.4 | Any distribution's Mutter has native mode; unverified versions are proven by qualification |
| NVIDIA card node without connectors (`nvidia_drm modeset=1`) | GRID V100D-16Q vGPU, driver 580 | `no_gpu_card`, `gpu_has_connectors` |
| `vkms` module | in the EL10 kernel package | Ubuntu ships it in `linux-modules-extra-$(uname -r)`; otherwise `vkms_missing` |
| polkit with JavaScript rules (≥ 0.106) and Arcen's rule (`05-arcen-remote-session.rules`) | polkit on EL10 | Ubuntu 22.04's polkit 0.105 has none (`polkit_no_rules`); `polkit_rule_missing` until the installer (or `arcen-pier polkit-rule install`) writes the rule |
| `61-mutter.rules` with a `vkms` ignore line | `/usr/lib/udev/rules.d` | `/lib/udev/rules.d` on split-`/usr` systems; a distribution without the file or the line needs no override |
| display manager | GDM 47 via `display-manager.service` | Any display manager behind that alias is detected; without one there is nothing to defend against |
| logind seats without VTs | systemd 257 | systemd has had them since multi-seat support |

Only EL10, GNOME 49 and an NVIDIA vGPU are verified end to end. Other
distributions are refused, or proven, by the same facts and the same
qualification proof.

**Limits.**

- One native desktop per host: a card node belongs to one seat, and
  kernel 6.12 offers one `vkms` device without configfs.
- The disconnected-idle timeout ends a native desktop the same way as a
  headless one, when the next connection arrives (not on a timer). The
  native seat is held until then.
- Grading and HDR are unchanged: Mutter ≥ 50 only.

**Future simplification (upstream).** Native mode could run with zero
connected outputs when persistent virtual monitors are requested. Mutter
would set `needs_outputs` false for that mode, as headless already does
(`meta-backend-native.c`, `meta-monitor-manager-native.c`). The seat would
then need only the NVIDIA card node and the tablet: no `vkms`, no rules
override, and no per-host `vkms` limit.

GDM could skip seats whose master device carries a property, for example
`GDM_IGNORE_SEAT`, in `ensure_display_for_seat`. That would remove the
display-manager hazard.

Headless libinput with a seat (mutter!5224 plus its three fixes, the lab
seat pen above) remains the route to several pen desktops on one GPU.

Lab evidence (Rocky Linux 10.2, stock Mutter 49.4, GRID V100D-16Q,
2026-10-20):

- **Real Deck GUI** (real-screen harness, synthetic tablet events, Speed,
  over the VPN link):
  - Firefox 140 and Chrome 155 (Wayland) report `pointerType: pen` for
    every inked sample, with 32 pressure levels, tilt ±45/±36 and no
    pointer-type flips.
  - Input → host event: 21 / 26 ms p50 (mouse 17 ms). Motion-to-photon:
    122 / 123 ms p50 (mouse 101 ms). Glass 54–60 fps.
  - Exactly one cursor in every frame.
  - hardwaretest.org shows Pen with pressure and tilt in both browsers.
- **Capture:** capenc served Speed at 60–61 fps over DMA-BUF
  (`gpu-convert`).
- **Lifecycle:** an idle-expired desktop's seat was taken apart and a new
  native desktop built in about 10 s.
- **GDM:** its login-screen attempts on the new seats failed as designed.

#### The Pier side (`input::wayland`)

An attachment chooses its injector once, from the session's facts
(`input::choose_input_backend`): uinput on an Xorg seat, or for a session
whose metadata names a Wayland agent, `WaylandInjector` on that session's
socket. A Wayland session never falls back to uinput, which would reach the
physical seat; a Wayland session whose input facts are missing or
inconsistent, or whose socket cannot be proven, refuses the attachment.
Before use the Pier (root) checks that the socket lies directly inside the
session's private runtime directory and that both belong to the session
user with no group or other access; after connecting, that the peer's uid is
the session user's and its pid the session agent's; and that `hello` names
the generation and streams the agent reported at start, keyboard and
pointer device types, a started `RemoteDesktop` session and the D-Bus
backend.

`WaylandInputPlanner` turns Deck messages into commands without I/O: Qt key
identifiers to evdev keycodes through the shared Linux table, the modifier
mask reconciled first, lock states toggled only on change, idempotent edges
(a Deck autorepeat of a held key sends nothing; Wayland clients repeat by
themselves); a Latin-1 character the keycode table lacks goes by keysym,
which Mutter delivers only when the session's keyboard layout has it (it
accepts and drops one it lacks); normalised positions to the primary
stream's pixels with the generation stamped; line scroll to discrete steps
and precise (point) scroll to continuous units (10 per detent, the Deck's
positive `dy` is up, Mutter's is down); relative motion as
`pointer_relative`. Typed pen is planned onto the primary stream as a `pen`
command only when the session's `hello` reports a bound emulated stylus and
the session serves one display; otherwise it is refused, and the hello
advertises pen, pen sub-capabilities, barrel buttons and local pen
termination unavailable, with the session's reason in the tablet-mode
result (a multi-display session's pen samples are region-scoped, which the
emulated stylus does not take yet). Mouse compatibility stays available, as
on a Windows host without a pen device. Region input is available exactly
when a multi-monitor topology is committed (see
[Multi-monitor](#multi-monitor-wp-multimon)): the shared
`RegionInputPipeline` maps a region-local position to the desktop, and
`arcen_input::locate_output` puts it on the output that shows it, in that
output's stream pixels and the current output generation; a point in a gap
between outputs is refused. With several outputs a normalised position
spans the whole desktop's bounding box.

Commands go through one ordered `InjectionQueue` per attachment and one
connection, so injection order is arrival order on the input lane. Motion
directly behind motion of the same kind is coalesced (absolute positions of
the same output and generation replace each other; relative and continuous
scroll deltas sum); edges are never merged, dropped or reordered. The queue
holds 512 commands; a full queue (Mutter stopped answering) is replaced by
one `release_all`, the planner forgets its held state, and the call reports
`QueueCollapsed`, so a release is never lost silently. `PeerLaneEvent::Lost`,
an input-lane supersession and the attachment's end call `reset_held`, which
queues `release_all`; dropping the injector queues one more and closes the
connection after at most 2 s of delivery, and the agent releases whatever
the connection still held.

Lock keys are reconciled by the session, not toggled by the planner: the
agent tracks the Caps, Num and Scroll Lock states its compositor has (a
headless session has no other keyboard), and `locks` toggles only what
differs. Tracking starts from the state the compositor started with: Caps
and Scroll Lock off, and Num Lock as GNOME remembered it. GNOME remembers
Num Lock by default (`org.gnome.desktop.peripherals.keyboard
remember-numlock-state`), Mutter 49 turns it on as its seat starts when
`numlock-state` is true and writes every change back while it remembers
(`meta-seat-impl.c`), so the agent reads both keys from the session's
settings when it starts (`LockState::at_session_start`). A desktop that
pins `remember-numlock-state=false` starts with Num Lock off and needs
nothing more. Proven on Rocky 10 (2026-10-18): with `numlock-state=true`
the session started with Num Lock on, one `locks` turned it off (Mutter
wrote `numlock-state=false`), and a second toggled nothing. The planner sends the Deck's lock
states when they differ from what it last sent, or when that is unknown:
after a queue collapse or a reconnection, which may have dropped a lock
command, its lock states are unknown and the next key event sends them
again. A key event for a lock key itself (Caps Lock pressed on the Deck)
does not reconcile that lock first, which would toggle it twice; its state
is unknown afterwards and the next key event reconciles it.

A connection that breaks or stops answering (no reply within 5 s) does not
end input for the attachment. The delivery task drops what was queued
(the session releases what the broken connection held), starts a new queue
epoch so nothing planned before the break reaches the new connection, and
reconnects with the same target, re-running the whole proof, after 250 ms,
1 s and 2 s (each attempt bounded at 5 s). Meanwhile input is refused with
`InjectorReconnecting` and counted (`input_unavailable_drops`), not
delivered late; the planner forgets its held state on the epoch change. A
connection the session superseded is told so before it is closed (an
unsolicited `superseded by a newer input connection` reply, which the
injector also watches for while idle) and does not reconnect, since that
would take the input back from the newer attachment. When reconnection
fails, or the connection was superseded, the injector's status becomes
`Lost` and the Pier ends the attachment with the close reason "session
input could not be restored" or "session input was taken by another
connection" (`SessionEndReason::InputLost`); there is no protocol message
that tells a Deck mid-session that input is unavailable. State changes are
logged once; refused events only at trace level. The input lane and the control lane are not
ordered: the only coordinate barrier is the generation each coordinate
carries, checked again by the agent; a Wayland session has one output
generation for its lifetime.

`WaylandInputProof` reports what `hello` proved: keyboard, pointer and touch
from the device types, pen and EIS missing, and `proven_facts` upgrades
`WaylandCapabilityFacts::input_injection` once keyboard and pointer are
available. Like every session proof, it does not change the Pier's
start-time selection.

#### Committed text (`text`, `keymap`)

The Deck sends what its input method commits (dead-key results, IME
conversions, emoji) and keystrokes the session's layout would type
differently as `text_commit` (WIRE.md, "Committed text"). Mutter types a
keysym through `RemoteDesktop.NotifyKeyboardKeysym` only when the **active
layout group** of its keymap has it at level 0, 1 (with a `Shift_L` key) or 2
(with an `ISO_Level3_Shift` key), and drops anything else silently
(`meta-virtual-input-device-native.c` `pick_keycode_for_keyval_in_current_group`,
gnome-49; gnome-remote-desktop's RDP Unicode input has the same limit). So the
session proves a character typeable before it types it:

- **Keymap.** The agent reads the compositor's keymap as a plain Wayland
  client (`session::wayland::wl_keymap`: `wl_seat` → `wl_keyboard.keymap`,
  kept open, so every keymap change arrives), compiles it with the system's
  libxkbcommon loaded at run time (`xkbcommon-dl`, `session::wayland::xkb`)
  and indexes it with the shared `arcen_input::text::KeymapIndex`, which
  reproduces Mutter's lookup. The keyboard is requested only after the seat
  advertises the keyboard capability: asking a headless Mutter 49.4 seat for
  its keyboard earlier crashes gnome-shell (`seat_get_keyboard` →
  `send_keymap` → `mtk_anonymous_file_open_fd`, seen in the lab).
- **Active group.** A single-group keymap needs nothing. Otherwise the group
  is the position of GNOME's active input source (the first `mru-sources`
  entry still in `sources`) among the keymap's groups. Mutter 49.4 compiles
  an `xkb_symbols "(unnamed)"` section, so groups are matched by name: each
  group's name (`Norwegian`, `English (UK)`) against the layout descriptions
  in the system's `rules/evdev.xml` (`arcen_input::text::XkbRegistry`); a
  named section (`pc+no+us:2+inet(evdev)`) is used when present. Mutter's
  keymap also carries the system layout (`localectl`) as an extra group
  after the user's sources. An IBus source or an ambiguous match is
  unknown, never guessed: typing a level-3 keysym from the wrong group
  makes Mutter press the key without its modifier, and the session types
  the wrong character (seen in the lab before groups were matched by name).
- **Typing.** `text` plans the commit with the shared planner: characters on
  the active group go by keysym, with held modifiers released and Caps Lock
  turned off while they are typed (Mutter would otherwise type the other
  case or a shortcut), then restored through the same held-input tracking
  as every key. Characters the layout lacks go through the compositor's
  input method when its EIS seat offers `ei_text` (below), and are otherwise
  refused. Without a readable keymap the agent types by keysym unverified
  and says so. The reply carries a `TextOutcome` (counts, refused
  characters, reason, how the group was found); every reply carries the
  keymap generation, and the Pier fetches the layout (`keymap`) when it
  changes.
- **`ei_text`.** When the compositor offers `ConnectToEIS`, the agent keeps
  one EIS connection (`session::wayland::eis_text`, `reis`) that announces
  `ei_text` (libei 1.6), binds only the text capability when the seat offers
  it, and commits each chunk (≤ 254 bytes) as
  `start_emulating`/`utf8`/`frame`/`stop_emulating`. Mutter MR !4996
  (open, not in any release on 2026-10-18) turns that into an input-method
  commit. Mutter 49.4's EIS does not implement `ei_text`: the handshake
  negotiates `ei_callback`, `ei_connection`, `ei_device` v2, `ei_pingpong`
  and `ei_seat` only, so on Rocky 10 such characters stay refused and the
  hello's text facts name that verdict (`eis_error`). The client answers
  `ei_device.done` with `ready` (required from `ei_device` v3), and was
  proven against libei's own `eis-demo-server` (main `a9bf31da`), which
  printed the committed `æøå`, `😀` and `日本語` (the ignored test
  `libei_demo_server_receives_committed_text`).
  This connection is the session's single EIS owner; other emulated devices
  bind on it.

On the Pier side `WaylandInjector::text_commit` validates the commit and
queues it as edges: one `text` command per part of at most 64 characters
(`arcen_input::text::text_parts`), rate-limited to 64 parts per second.
Typing a character costs the session a press and a release, each a
compositor round trip, so a `text` command's budget is the fixed 5 s plus
50 ms per character, and one part fits well inside it. The parts' outcomes
are added up and reported once, after the last part. A layout request
answered by the superseded notice ends delivery as superseded, like any
other command. The hello advertises
`text_commit` only when the agent's facts carry `text` (an older agent says
nothing, so the Pier refuses committed text), and `text_commit_unicode`
only when the `ei_text` route is up. Results go back to a Deck that
advertised committed text as `text_commit_result` (only refusals and
unverified typing) and the layout as `host_keymap`, on the input lane
(`net/lane_workers.rs`, a newer layout replaces a waiting one). Text is
never logged, only counts. An Xorg session's uinput keyboard types key
positions only and advertises committed text unavailable.

### Cursor

A desktop chooses how its streams report the cursor when it opens, and
keeps it for its life (every remote-monitor set recorded again, after a
resize or after Mutter closed the sessions, uses the same mode). The choice
is the shared `resolve_stream_cursor_mode` from the opening Deck's
`AuthResponse` and the host's qualification record:

| Deck preference | Deck draws cursor images (`cursor_image_v1`) | Record proves `cursor_metadata` | Stream `cursor-mode` | Served |
| --- | --- | --- | --- | --- |
| local | yes | yes | metadata | `cursor=local`, the Deck draws the host's cursor image |
| local | no (older Deck) | any | hidden | `cursor=local`, the Deck draws its own pointer |
| local | yes | no | hidden | `cursor=local`, the Deck draws its own pointer |
| host | any | any | embedded | `cursor=host` |

A Deck that draws the cursor never gets a second one composited into the
video: its own pointer moves at its display's rate and one in the pixels
would trail it at the video's. Without metadata the Deck's pointer keeps its
default shape.

The mode travels launcher (`OpenRequest.stream_cursor`) → agent
(`--cursor-mode`) → `WaylandAgentReady.cursor_mode` → the capture
hand-off. capenc takes cursor truth from the hand-off's `cursor-mode` for
the whole stream (`ScreenCastCursorMode::cursor_authority`: `embedded` is
the host's, `metadata` and `hidden` the Deck's) and refuses a request whose
`cursor=` disagrees; the Pier sets `cursor=` from the hand-off. A later
attachment whose Deck asks otherwise is told the desktop's truth in
`cursor_mode_result` with a reason. A Deck that asked to draw the cursor
keeps drawing it even then: the Mac's own pointer is never traded for one
that moves at the video's rate. Mutter hides its pointer on every tablet
event, so while the Deck's own pen drives the pointer (Tablet support,
light) the metadata reports it hidden; the Deck keeps the Mac's cursor then
(`arcen_input::cursor::LocalPenCursor`).

**Embedded.** Mutter draws the cursor into the frames, so cursor motion on a
still desktop produces frames.

**Metadata.** Mutter reports `SPA_META_Cursor` (GNOME 40 and 49 offer it
only for a 384×384 bitmap). Its dialect, read from Mutter's
`meta-screen-cast-*-stream-src.c` and matched to gnome-remote-desktop, its own
consumer, which records virtual monitors the same way:

- cursor id 0: the cursor is not on this output, or the pointer is not
  visible. A client that sets no cursor surface (GTK's `none`) is reported
  this way on Mutter 49.4, not as an empty sprite. When no output of the
  desktop reports the cursor, the Deck hides its own; when another output
  does, that output's sprite is shown.
- id 1 with `bitmap_offset` 0: position only, the sprite is unchanged.
- id 1 with an all-zero `spa_meta_bitmap`: an empty sprite, which hides the
  pointer. A new headless desktop starts this way until a sprite exists.
- id 1 with a bitmap: a new sprite, premultiplied `SPA_VIDEO_FORMAT_RGBA` in
  stream pixels, with its hotspot.

Mutter writes a sprite only when it changes and when its stream source
starts, never for a new consumer of the same stream. capenc therefore folds
cursor metadata from every buffer not marked corrupted (refused ones and
ones before the format included), and on framed-v2 stdout
(`arcen_media::capenc_stream`, written by every Wayland epoch: NVENC eight-bit,
wide Grading/HDR and OpenH264, including the Pier's runtime software
fallback) reports a cursor event when the cursor
enters or leaves the output or its sprite changes, never for motion alone.
The Pier applies the events to the desktop's
`arcen_input::cursor::DesktopCursor`, kept for the desktop's life
(`media::wayland_cursor`), so a capture restarted after a resume or a
capture restart, which sees positions only, keeps the remembered sprite.
Each attachment's input-lane worker sends the newest state as a
`CursorUpdate` record (ADR 0014 amendment 11) to a Deck that declared
`cursor_image_v1`, with the PNG inline only when the Deck does not hold that
image, or the `cursor_shape` name `default`/`hidden` to an older one. The
Deck shows the image as its native cursor at the video's scale. Pointer
motion on a still desktop produces cursor-only buffers, not frames.

**Several outputs.** Every output's stream is recorded in the desktop's
mode: the initial set, outputs added or removed in place, a replacement set
and the wide (Grading/HDR) casts. Each output's capenc reports its own
stream's events to the same `DesktopCursor`, which numbers the outputs in
the order they first report, so the record's `output` is stable for the
desktop but is not a session monitor id. When the pointer crosses outputs,
the one it left reports id 0 and the one it entered reports it, in either
order, so on a desktop with several outputs the Pier sends "no output
reports it" as hidden only if it still holds 80 ms later
(`media::wayland_cursor::CROSSING_GRACE`). The sprite of the output that
last reported the cursor is shown, or the last sprite any output reported
when that output's capture never saw one. The Deck shows the
same cursor in every display window (each secondary viewport scales the
image to its own video), and hides its own cursor there too when the host
draws it. Removing the output the pointer is on leaves its last report until
Mutter moves the pointer onto a remaining output, which then reports it.

The Pier never starts its X11 cursor watcher for a Wayland session: it would
watch whatever X display the configuration names, not this session.

`arcen-pier wayland qualify` proves `cursor_metadata` on a short second
desktop recorded with metadata while the pointer moves (a sprite bitmap must
arrive); the Speed measurement keeps its embedded cursor, whose motion makes
frames.

### X11 applications (Xwayland)

Code: `hosts/linux/src/session/wayland/xwayland.rs`, the X11 parts of
`stack.rs`, `agent.rs`, `mod.rs` (`ReadyX11`, `resolve_configured_x11_apps`)
and `qualify.rs`, and the pure policy in `arcen_session::wayland_x11`.

X11-only applications run in a Wayland session through the compositor's
own Xwayland. Examples are Qt 5 without its Wayland plugin, Motif, Java
AWT/Swing, older Electron and many OpenGL tools. The operator setting
`platform.session.wayland_x11_apps` decides whether a session gets one:

| Setting | Wayland sessions |
| --- | --- |
| `auto` (default) | With Xwayland when `/usr/bin/Xwayland` is installed **and** the host's qualification record proves it (`facts.x11_apps`, written by `arcen-pier wayland qualify`: served display, managed screen **and** the session's pointer reaching X11 windows); otherwise `--no-x11`, with the reason logged at start (`xwayland_not_installed`, `not_qualified`, `qualification_failed`) |
| `on` | With Xwayland; the Pier refuses to start, naming what is missing, when it is not installed or not proven |
| `off` | `--no-x11`, as before this setting existed |

The decision is the shared `resolve_x11_apps`. It is made once at start,
logged as `X11 applications in Wayland sessions: …`, and passed to the
launcher (`--wayland-x11-apps`) and the agent (`--x11-apps`).

**Mutter owns the X server.** Without `--no-x11`, Mutter does the following
([`meta-xwayland.c`](https://gitlab.gnome.org/GNOME/mutter/-/blob/gnome-49/src/wayland/meta-xwayland.c),
[`meta-wayland.c`](https://gitlab.gnome.org/GNOME/mutter/-/blob/gnome-49/src/wayland/meta-wayland.c)):

- Picks two free displays: a public `:n` and a private one "for managed
  services".
- Creates `/tmp/.X<n>-lock` holding the compositor's own pid, using the lock
  protocol Xorg uses. It skips a display whose lock or socket exists and
  unlinks a lock whose pid is dead.
- Binds `/tmp/.X11-unix/X<n>` and an abstract socket.
- Writes an MIT-MAGIC-COOKIE-1 to `$XDG_RUNTIME_DIR/.mutter-Xwaylandauth.*`,
  which is inside the private runtime directory.
- Runs `Xwayland :n -rootless … -auth <cookie>`.
- Exports `DISPLAY`, `XAUTHORITY` and `GNOME_SETUP_DISPLAY` to what it
  launches. Because the private bus has no `org.gnome.SessionManager`, it
  also exports them to the bus's activation environment
  (`UpdateActivationEnvironment`).

The stack hands the same three variables to the session services it starts
itself ([GNOME login services](#gnome-login-services-wp-gnomesession)): once
the displays are discovered, `gsd-*` and XDG autostart programs get
`DISPLAY=:n`, `GNOME_SETUP_DISPLAY=:n+1` and `XAUTHORITY` naming the one
regular `.mutter-Xwaylandauth.*` file the session user owns in the runtime
directory, never followed through a link. This is what GNOME's own session
does, and it lets `gsd-xsettings` serve XSETTINGS to X11 toolkits.

Display numbers cannot collide with an Xorg session, another user's
Xwayland or another Pier's: every server takes its number through that
O_EXCL lock-file protocol, and Mutter tries up to 50 numbers.

Arcen proves the server rather than trusting the socket:

1. After `wayland-0`, the stack finds the two lock files owned by the session
   user that hold the compositor's pid (`xwayland::discover`).
2. The agent finds the compositor's Xwayland child in the session scope. Its
   command line must name the public display and Mutter's cookie in the
   runtime directory.
3. The agent connects as an X11 client through a descriptor-checked socket,
   using that cookie and `x11rb`. It must be served, and the root window must
   carry `_NET_SUPPORTING_WM_CHECK`.
4. The agent reads where glamor renders from the libraries Xwayland mapped:
   `nvidia-gbm`, `mesa-gbm`, `software` or `unknown`.

Qualification adds the input proof a session cannot make for itself
(`x11_pointer`, after GNOME Shell's Activities overview is left, because
the overview keeps the pointer). It maps an override-redirect X11 window
over the output. It then injects absolute motion to two points through the
session's input socket, the Pier's own path. The X server's pointer
(`QueryPointer` on the root) must land within two pixels of each point.
`XwaylandProof::proven` requires the display, the window manager and the
pointer, so `auto` never serves X11 applications that cannot be clicked
where the Deck points.

The ready message carries this as `x11` (`ReadyX11`): `ready`, `failure`,
`displays` (public, private, lock pid), `xauthority` and `glamor`. A failed
proof is a visible, logged degradation (`X11 applications are not served in
this session; Wayland applications are unaffected`), never a failed desktop.

**`DISPLAY` never enters the Pier's own session environment.** Applications
get it from Mutter. The Pier's Wayland environment (`build_wayland`,
`with_wayland_session`) keeps no `DISPLAY`/`XAUTHORITY`. This keeps every
X11-only Pier path off in a Wayland session: the cursor watcher, deskside,
XShm/NvFBC capture and the X11 clipboard child. A unit test pins it.
Clipboard, cursor and input reach X11 windows through Mutter:

- Mutter bridges X11 `CLIPBOARD`/`PRIMARY` to its own selection, so the
  [clipboard](#clipboard) bridge reaches X11 applications without X11 code.
- Xwayland sets its cursors on Wayland surfaces.
- Mutter routes `RemoteDesktop` input to the focused X11 window.
- ScreenCast records the composited desktop, Xwayland windows included.

**Always running.** Mutter starts Xwayland on demand only when the
compositor runs in a systemd *user unit*: `sd_pid_get_user_unit` succeeds
(`meta_context_main_get_x11_display_policy`, `meta-context-main.c`; GNOME 40
has the same rule in `main.c`). GDM's `org.gnome.Shell@wayland.service` is
such a unit. Arcen's compositor runs in the logind session scope, so the
policy is *mandatory*:

- Xwayland starts with the session.
- It costs about 82 MB RSS and 17 MiB of GPU memory per session (measured on
  Rocky 10.2, GNOME 49.4, GRID V100D).
- If Xwayland dies, the compositor exits, and the session ends through the
  usual member-exit teardown, as an Xorg crash ends an X11 session.

Running the compositor as a user unit would give on-demand start and
`autoclose-xwayland`, but it would move the desktop out of the session scope
that Arcen's release proof owns. That is a tracked follow-up, not a
limitation of Mutter.

**Teardown.**

- On a clean stop Mutter removes its sockets, lock files and cookie itself.
- A compositor that dies without its own shutdown (SIGKILL) leaves the
  sockets and lock files in the shared `/tmp`.
- The agent (its stop and `Drop`, as the session user) and the launcher (as
  root, from the displays in the ready message) remove exactly those. No
  path in `/tmp` is ever followed (`remove_session_artifacts`):
  - Each directory is opened `O_NOFOLLOW` and must be root-owned and sticky.
  - Each entry is `fstatat(AT_SYMLINK_NOFOLLOW)`-checked: a socket or regular
    file owned by the session user and named for one of the session's
    displays.
  - The lock is read first. A display counts as the session's only while its
    `/tmp/.X<n>-lock` is the user's regular file holding the compositor's
    pid. A freed number may already serve another desktop of the same user,
    whose lock names its own pid or is not written yet; its socket is left
    alone.
  - Only then is the socket, and after it the lock, `unlinkat`-ed through
    the directory descriptor. A socket that cannot be removed keeps its lock.
- The release proof includes "no X11 socket or lock of the session remains".
- An agent killed together with its compositor before it reported ready
  leaves the stale lock to Mutter's own stale-lock handling and to
  `systemd-tmpfiles` at boot (`/usr/lib/tmpfiles.d/x11.conf`).

**SELinux.**

- `Xwayland` (`xserver_exec_t`) enters `xserver_t` from the session's
  `unconfined_t` by the policy's own transition.
- Its sockets and lock files are `user_tmp_t`.
- No policy change is needed; see
  [Linux Pier under SELinux](../security/linux-selinux.md).

**Security.**

- Cross-user isolation:
  - The socket in `/tmp/.X11-unix` is owner-only.
  - The abstract socket is reachable from the network namespace, but the
    server refuses a client without the cookie (proven: `Authorization
    required`).
  - The cookie is 0600 in the 0700 runtime directory.
- Inside one session, X11 offers no isolation between X clients. That is the
  same as the Xorg provider. Rootless Xwayland cannot see Wayland windows.
- Xwayland receives X.Org security advisories; for example, the 2025-02-25
  advisory fixed CVE-2025-26594…26601 in Xwayland 24.1.6. Keep it patched,
  or set `off`.
- X clients that synthesize input with XTEST go through the RemoteDesktop
  portal (`-enable-ei-portal`), which nobody approves on a headless desktop.

**What X11 applications do not get on Wayland with the NVIDIA driver.**
These limits come from the driver, Xwayland and the Wayland protocol, not
from Arcen. They are listed in NVIDIA's
[Wayland Known Issues](https://download.nvidia.com/XFree86/Linux-x86_64/580.105.08/README/wayland-issues.html)
(580.105.08):

- GLX front-buffer rendering, indirect GLX, and hardware overlays for GLX
  applications.
- Stereo rendering, swap groups, frame lock/genlock and SLI Mosaic. No
  Wayland compositor supports these; some need `VK_KHR_display`.

Accelerated GLX/Vulkan on Xwayland itself needs `nvidia-drm modeset=1`
([NVIDIA, chapter 40](https://download.nvidia.com/XFree86/Linux-x86_64/580.105.08/README/xwayland.html)).
Without a DRM device, Xwayland's glamor has nothing to render on, and the
record says `software`. That is expected on GNOME 40's surfaceless headless
compositor (not yet measured).

**Pointer input on `RecordVirtual` outputs needs a consumer of the stream
(GNOME 49).** `NotifyPointerMotionAbsolute` is relative to a stream. For a
`RecordVirtual` stream Mutter maps the position only while that stream is
enabled, which means a PipeWire consumer has it streaming. Otherwise the
motion is dropped silently, the D-Bus call still succeeds, and the
`remote-desktop` debug topic logs "Dropping early absolute pointer motion".
The sources (Mutter 49.4):

- `meta_screen_cast_virtual_stream_transform_position`
  (`src/backends/meta-screen-cast-virtual-stream.c`) returns false unless
  the source is enabled.
- `on_stream_state_changed` (`meta-screen-cast-stream-src.c`) enables the
  source on `STREAMING` and disables it on `PAUSED`, `UNCONNECTED` and
  `ERROR`.
- `handle_notify_pointer_motion_absolute`
  (`meta-remote-desktop-session.c`) skips the motion when the transform
  fails.
- A startup monitor's stream maps through the logical monitor
  (`meta-screen-cast-monitor-stream.c`) whether or not it is consumed.

This applies to Wayland and X11 windows alike. A Deck's capture is always
consuming its outputs, so served sessions are unaffected. Pointer motion
sent between Deck connections is dropped, which is harmless.

A proof that drives the pointer must consume the stream first. The
qualification's `x11_pointer` step runs the session's own capture for the
duration: capenc started as a Deck's session would be, with the encoder the
host serves (OpenH264 where no native encoder opened), returning only after
its first access unit. The harness keeps a size-fixed `gst-launch-1.0` consumer through
its input stages.

Measured on Rocky 10.2 with Mutter 49.4 and Xwayland 24.1.9:

- Without a consumer, the X server's pointer stayed at `(0, 0)`. The
  compositor sent Xwayland `wl_pointer.enter(…, 0, 0)` and no motion, and
  the harness's Wayland drag failed too.
- With a consumer, X11 and Wayland windows follow exactly: `(576, 432)` and
  `(1344, 648)` at 1920×1080, and likewise at 2560×1600 and 3840×2160.

A consumer that fixes no size (bare `pipewiresrc`) negotiates Mutter's
default 1280×720 (`DEFAULT_SIZE` in `meta-screen-cast-stream-src.c`; a
virtual source states no size of its own). That resizes the remote monitor
in place, so every lab consumer fixes the output's size, as capenc does.

The following are not provided yet:

- X11 applications at a desktop scale above 1. That needs Mutter's
  experimental `xwayland-native-scaling` and belongs to the HiDPI work; the
  agent sets scale 1 today.

### Clipboard

Text (UTF-8) and PNG images cross both ways under the same host policy,
negotiation, limits and wire protocol as on dedicated Xorg. The Deck needs no
change. Mutter provides the clipboard on a `RemoteDesktop` session:

- methods `EnableClipboard`, `SetSelection`, `SelectionRead`,
  `SelectionWrite` and `SelectionWriteDone`;
- signals `SelectionOwnerChanged` and `SelectionTransfer`, with descriptors
  passed over D-Bus.

The interface text is identical in GNOME 40, 49 and `main` (it was added by
Mutter MR !1552 for GNOME 40). GNOME 51 also exposes it as
`org.gnome.Mutter.Clipboard` (MR !4767) and keeps the `RemoteDesktop`
methods.

- **Its own session.** The agent creates one `RemoteDesktop` session for the
  clipboard alone (`session::wayland::clipboard::MutterClipboard`). It never
  starts it and never links it to a `ScreenCast` session. Mutter serves the
  clipboard on a session that was not started (proven on GNOME 49 and 40).
  Output reconfiguration and stream recovery replace the paired recording
  sessions, never this one, so a resize loses neither the selection nor a
  transfer in flight. A session that was never started cannot be stopped;
  Mutter closes it with the agent's bus connection.
- **Proven at start, enabled once.** The agent introspects all six methods
  and both signals, then enables the clipboard, before its ready message.
  Only then does `WaylandAgentReady.clipboard` name `arcen-clipboard.sock`.
  Without it, the Pier advertises the clipboard `disabled` for that desktop
  (shared `ClipboardHostFacts`, reason `no_native_bridge`). The clipboard is
  never disabled: on GNOME 40 through 50, `DisableClipboard` disconnects the
  owner signal but leaves the session marked enabled, so it could never be
  enabled again (Mutter fixed this in the GNOME 51 refactor).
- **Read only while a Pier is attached.** Signals are subscribed per Pier
  connection. The owner present when the Deck attaches is not sent, as on
  Xorg, where the child sees only later changes. When the connection ends,
  the bridge releases the selection it took for the Deck (`SetSelection`
  without names, which unsets only its own source).
- **The socket** `arcen-clipboard.sock` follows the input socket's rules.
  It is mode 0600 in the private runtime directory and accepts root or the
  session user. One connection is served at a time, and a newer one
  supersedes the older one. The Pier proves it before use: the socket and
  directory are private (`socket_proof`), the peer is the agent's uid and
  pid, and the ready line names the same user and process. On it, the Pier
  speaks the dedicated-Xorg child's bridge protocol unchanged: a WebSocket
  whose first message is the negotiated policy and directions, then
  `clipboard_data` offers and chunk frames. Dispatcher, bulk lane,
  reassembly, `clipboard_transfer_failed` and validation are one path for
  both providers.
- **Decisions are shared.** `arcen_media::clipboard::bridge::SelectionBridge`
  decides what to read, ignore, offer and serve; the agent only moves bytes
  between Mutter's descriptors and that state machine. The rules:
  - Our own ownership (`session-is-owner`) is never read back.
  - The newest owner change cancels an older read; Mutter refuses parallel
    reads.
  - A read is bounded at `max_bytes + 1` with a 5-second progress deadline.
    An oversized copy is refused and counted, never truncated, and the
    session goes on.
  - At most two paste requests are served at once.
  - Payloads are scrubbed when dropped.
- **Formats** come from `arcen_media::clipboard::formats`, the same table as
  the Xorg child:
  - The host reads text first: `UTF8_STRING`, `text/plain;charset=utf-8`,
    `text/plain`, `TEXT`, then `STRING` (latin-1); after text, `image/png`.
  - A Deck item is offered as `text/plain;charset=utf-8`, `UTF8_STRING`,
    `text/plain` and `TEXT` (plus `STRING` when the text fits latin-1), or as
    `image/png`.
  - Mutter sends `SelectionOwnerChanged`'s `mime-types` as a one-element
    structure `(as)`, although its documentation says `as`; both are
    accepted.
- **Reading never waits on writing.** The agent reads and writes the bridge
  in separate futures. On the Pier side, `ClipboardBridgeLink` hands client
  items to its own writer task through a latest-wins mailbox, and the
  attachment's main loop only offers and reads. Without this, a host item
  streaming to the Pier while a client item streams to the agent leaves both
  ends writing into full socket buffers (1 MiB chunks against a ~200 KiB
  socket buffer), and neither reads again. A helper that takes nothing for
  5 s (`BRIDGE_SEND_TIMEOUT`) turns a Wayland bridge off for the attachment.
  The lab proved 4 MiB both ways at once on GNOME 49.
- **Failure is local.** A bridge that is missing or lost turns the clipboard
  off for that attachment, and the session goes on. The logs record
  `clipboard_bridge_unavailable` or `clipboard_bridge_lost`. There is no wire
  message that withdraws a negotiated clipboard, so the Deck is not told; a
  notice would be a protocol addition (follow-up).
- **Residual copy.** Mutter's clipboard manager copies every new owner
  eagerly: text up to 4 MiB, images up to 200 MiB. It reinstalls that copy
  when the owner goes away. An item the Deck pasted into the session
  therefore stays on the session's clipboard after the Deck detaches, as on
  Windows and macOS and unlike Xorg
  ([clipboard privacy](../security/clipboard-redirection.md)).

#### Clipboard lab evidence (2026-10-18)

The hosts:

- **GNOME 49:** Rocky 10.2, Mutter 49.4, SELinux enforcing, with
  `RecordVirtual` remote monitors.
- **GNOME 40:** Rocky 9.7, Mutter 40.9, startup monitors.

On each, a development Pier was built from this branch, qualified into a
record of its own, and run beside the installed service. The "application"
in the session was a separate `RemoteDesktop` clipboard client: a real
`MetaSelection` owner and reader. The Deck was the macOS `media-smoke` with
`ARCEN_MEDIA_SMOKE_CLIPBOARD_*`. Items were compared by size and equality.

| Check | GNOME 49 | GNOME 40 |
| --- | --- | --- |
| A clipboard-only `RemoteDesktop` session, never started, carries the clipboard (spike, then the agent's proof) | yes | yes |
| `examples/wayland_clipboard_harness` (both directions, latin-1 `STRING`, PNG, oversize refused, supersession) | 9/9 ok | 9/9 ok |
| Deck → host text (42 B) and PNG (146 419 B), equal | yes | yes |
| Host → Deck text (40 B) and PNG (186 892 B), equal | yes | yes |
| Host copy of 9 MiB with an 8 MiB policy | refused, never sent; the session and later copies went on | same |
| Clipboard across a Windowed resize to 1600×900 (Deck item after it, host item after it) | yes (resize accepted, generation 2) | resize not offered (startup monitors) |
| A second attachment to the persisting desktop (no re-enable needed) | yes | yes |

The lab found three Mutter behaviours, and the bridge now handles each:

- On GNOME 40 through 50, `DisableClipboard` cannot be followed by
  `EnableClipboard`.
- `mime-types` arrives as the structure `(as)`.
- A reader gets a non-blocking pipe.

A further observation: when an application that owned the selection exits,
Mutter's clipboard manager reinstalls its copy, which is a new owner. The
same content is then sent again. Xorg with `gsd` does the same (seen in the
GNOME 40 Xorg regression run).

### Teardown

| Trigger | Path |
| --- | --- |
| Pier disconnect or shutdown | `close` to the launcher → SIGTERM to the agent only → agent releases held input, stops the D-Bus sessions, rolls the outputs back (session-manager clients told to stop, autostart programs and settings daemons stopped, compositor, WirePlumber, PipeWire, optional members, bus, then every remaining user process in the scope, unmount, remove the runtime directory, prove it) → launcher sweeps the scope as root and proves it again → PAM closes |
| Stop while the desktop is still starting (`close`, the Pier giving up on the open, SIGTERM, logind) | the launcher installs its stop handling before it validates logind or spawns the agent; a stop drops the start, which kills the starting agent (its stack follows by parent-death signal), then the same root-side release and PAM close run |
| Log Out inside the desktop (the shell's menu or `org.gnome.SessionManager.Logout`) | the shell's end-session dialog (Cancel is its default button; its 60 s countdown confirms) → on confirmation the session manager sends `EndSession` to its clients (5 s bound) and the agent ends the desktop as on a Pier stop (`user_logout`); the Deck sees the session end |
| An autostart program or settings daemon exits | logged; an `X-GNOME-AutoRestart` entry restarts (at most 3 times in 10 minutes); the desktop never ends for it |
| Compositor or stack member death | the agent sees the exit, tears down as above and exits; the launcher sees the agent exit and does the same |
| Mutter closes the ScreenCast/RemoteDesktop session | as compositor death |
| Logout / `loginctl terminate-session` | logind sends SIGTERM and SIGHUP to the scope; both launcher and agent handle SIGHUP as a stop request, so the orderly path runs |
| Xwayland death (X11 applications on) | Mutter treats its Xwayland as mandatory and exits; as compositor death. The X11 sockets and lock files it left are removed by the agent and the launcher (see [X11 applications](#x11-applications-xwayland)) |

The root-side release never acts on a path inside the user-writable runtime
directory with root privileges. Root only signals the scope's processes and
checks the outcome, and never by bare pid: each process is pinned with
`pidfd_open`, its owner and `/proc/<pid>/cgroup` are re-checked against the
session user and the session scope after it is pinned, and the signal goes
through `pidfd_send_signal`. A process that exited, or a recycled pid now
belonging to someone else, is never signalled. The agent's own sweep uses
the same function. Unmounting runs `fusermount -u -z` as the
session user (same privilege drop as the agent), and only for mountinfo
entries of type `fuse`/`fuse.*` whose `user_id` is the session user, so
`fusermount` itself refuses anything else; removal runs
`rm -rf --one-file-system` as the session user. If something still remains,
release reports failure and logind removes the directory with the user's
last session.

Start is bounded end to end: the agent's whole start (stack, verification,
D-Bus sessions, stream, input probe) has a 40 s budget, after which it
releases what it started; the launcher waits 45 s for the agent; the Pier
waits 60 s for a Wayland launcher (Xorg keeps 20 s). Each nested limit
outlasts the one inside it, so the Pier never kills a launcher that is still
inside a bounded start.

The launcher now bounds its tokio runtime shutdown, so a blocking stdin read
no longer keeps it — the logind session leader — alive after teardown; that
had held sessions in `closing` until the Pier closed the pipe.

### Provider selection

`capture.provider` is a preference: `auto` (the default), or the operator
overrides `xorg` and `wayland`; `wayland` is accepted at configuration load
only by a `wayland-provider` build (`WAYLAND_ADAPTER_BUILT`, passed to the
shared `validate_desktop_provider_for`; shared code has no `cfg`). At start
the Pier gathers facts — never a distribution name — and calls the shared
`select_desktop_provider` (ADR 0014 Amendment 4):

| Host | `auto` selects |
| --- | --- |
| An Xorg server is installed (the configured `platform.session.xorg_bin`, or `/usr/libexec/Xorg`, `/usr/lib/xorg/Xorg`, `/usr/lib/Xorg`, `/usr/bin/Xorg`) | dedicated Xorg, without probing anything Wayland or reading the record: an Xorg installation is never moved (`xorg_server_installed`) |
| No Xorg; compositor, logind, D-Bus, PipeWire, an accessible DRM render node, and a valid record proving the capture route and input | headless Wayland (`wayland_qualified`) |
| No Xorg and no DRM render node | refused: install an Xorg server, or a GPU driver with DRM/KMS (Intel i915/xe, AMD amdgpu, virtio-gpu, or for NVIDIA `nvidia-drm` with `modeset=1`); Arcen never changes driver configuration |
| No Xorg, DRM present, Wayland not proven | refused, naming each missing fact and the record's problem (missing, stale with what changed, malformed) and `arcen-pier wayland qualify --write` |
| No Xorg, a build without `wayland-provider` | refused: install an Xorg server |

`xorg` needs an installed Xorg server; `wayland` needs every Wayland fact
(an Xorg server may be present). An override that cannot be honoured
refuses to start; nothing ever falls back to the other provider
(`ProviderRefusal`). The resolved provider is what the launcher is told.

The live probe: `gnome-shell`/`mutter --help` must list `--headless` and
`--virtual-monitor`; logind; `dbus-daemon`; `pipewire` and `wireplumber`; a
`/dev/dri/renderD*` everyone may open (group-only access is unknown, no
node at all is missing). The session agent chooses its compositor in
preference order, `gnome-shell` before `mutter`: a probe that fails or
times out for an installed compositor stops the session only when that
compositor would have been chosen, so a slow `gnome-shell --help` never
silently selects `mutter`, and a failing `mutter` probe never blocks a
usable `gnome-shell`.

The capture route and input injection can only be proven by a session, so
they come from the record (`QualifiedHost::apply`, which never hides a fact
the live probe proved missing). Auto/Speed need a proven capture route
(`capture_route`: any truthful route, a labelled host copy included,
delivered an encoded first frame), not GPU import; `gpu_import` is reported
and proven only by a GPU import route. Every session still proves its own
first frame and input target; the record never stands in for them.

### Lab evidence (Rocky 9.7, GNOME Shell 40.10, Mutter 40.9, PipeWire 1.0.1)

`hosts/linux/examples/wayland_session_harness.rs` (with
`scripts/lab/wayland-input-client.py`) authenticates a temporary local user
through PAM and the real launcher, then checks each step:

```text
session_start: logind_session=… open_ms≈1750 compositor=gnome-shell
logind_session: Type=wayland Class=user Seat= Service=login Active=yes
virtual_output: Meta-0 1920x1080@60
first_frame: node=36 format=BGRx size=1920x1080 buffers=3
capenc_handoff: capenc parsed node=36 output=Meta-0 generation=1
                expected_size=1920x1080 cursor_mode=metadata,
                compositor/encoder GPU 0000:00:10.0 via renderD128 from the
                session's environment; 30 frames host-copy, 10 NVENC
                access units, upload 0.89 ms, read-back verified
audio_monitor: PULSE_RUNTIME_PATH=/run/user/<uid>/pulse source=auto_null.monitor
input_facts: hello generation=1 Meta-0 device_types=7 RemoteDesktop started,
             eis_offered=false; keyboard, pointer, touch available, pen and
             EIS missing; input_injection upgraded to available
first_key / pointer / keyboard / scroll / held_release / refusals:
             through the Pier's WaylandInjector (see Input)
compositor_death: session ended ≈1 s after SIGKILL of gnome-shell
logout: terminate-session ended the session ≈0.8 s, runtime directory removed
teardown: logind session, scope, runtime directory and mounts gone; the
          user manager stopped with the last session
```

It also runs `--slow-start` (the agent is stopped for 25 s mid-start; the
open still succeeds after about 25.6 s) and `--stop-during-start` (the
launcher receives SIGTERM while the agent is stopped mid-start with the
document portal's FUSE mount present; the open fails, and the logind
session, scope, runtime directory and mounts are gone and PAM is closed).

GNOME Shell 40 starts in the Activities overview, which takes pointer and
keyboard until dismissed; a remote desktop must expect that. GNOME 40
ScreenCast delivers frames only when something changes: an idle desktop
produces none, so a capture consumer must not treat a quiet stream as dead,
and the first-frame proof needs damage (the harness's client animates).

#### First login, the overview and the first key

On a user's first login GNOME Shell 40 opens its modal "Welcome to Rocky
Linux" dialog (shown when `org.gnome.shell welcome-dialog-last-shown-version`
is older than 40 and `gnome-tour` is installed) over the Activities overview.
Both take keyboard and pointer, exactly as on a console; the dialog explains
a harness run that received no input at all. Arcen does not suppress either:
they are the user's own desktop, the dialog records that it was shown in
the user's settings, and a Deck user dismisses both with Escape. The harness
does the same through the Pier's injector (`--dismiss escape`, the default:
two Escapes on a first login, one afterwards) and checks
`org.gnome.Shell.OverviewActive` until it is false.

The first key pressed after the overview closes does not reach the client
when the overview was closed without a key: through `OverviewActive=false`
(`--dismiss dbus`, two runs) or by clicking the client's window in it
(`--dismiss click`, three runs). `WAYLAND_DEBUG` shows why: the client has
no Wayland keyboard focus until that key event, and `wl_keyboard.enter`
arrives with the key already in its pressed-keys array, which GTK (like
other toolkits) does not treat as a press; only the release is delivered as
a key event. `RemoteDesktop` accepted the press, so the event reached Mutter;
the focus hand-over is GNOME 40's. When the overview is closed with Escape,
the Escape is the key that is consumed, and the next key arrives normally
(every Escape run). Arcen does not inject a synthetic key to force the
focus change. Whether a physical keyboard on a GNOME 40 console behaves the
same was not tested.

#### Input lab evidence (2026-10-17, same host)

The harness drives the Pier's `WaylandInjector` (root, real peer and `hello`
checks) into the GTK client; every run below passed, on a first login (the
welcome dialog: two Escapes) and on later logins (one):

```text
input_facts:  hello generation=1 Meta-0 1920x1080 device_types=7 started,
              eis_offered=false; keyboard/pointer/touch available, pen and
              EIS missing; proven_facts input_injection=Available
first_key:    'a' press and release after Escape dismissal
pointer:      absolute motion; button 1 drag with motion between press and
              release (one wl_pointer.button each way in WAYLAND_DEBUG)
keyboard:     Shift from the modifier mask gives 'B'; 'd' held 900 ms with
              three Deck repeats: one injected press, 15 client repeats,
              one release; 'æ' by keysym with layout `no` (dropped under `us`)
scroll:       wheel up gives up, wheel right gives smooth dx>0; precise 60
              points up gives smooth dy=-0.5
held_release: 'e' released on reset_held (lane loss); 'f' on injector drop;
              'g' when a newer injector superseded the old one, whose next
              command failed (lost); 'i' when a raw connection closed
refusals:     unknown output, stale generation (2), x beyond the stream;
              the Pier refused a peer whose pid is not the agent's
injector_loss: agent SIGSTOPped 6.5 s: command timeout, reconnecting, a key
              typed meanwhile refused and counted, the connection restored
              and proven, the next key delivered; neither the key written
              just before the stall nor the one typed during it ever arrives
injector_lost: agent stopped beyond every attempt: Lost(Unreachable) after
              about 23 s, later input refused as lost; the key written before
              the stall never arrives
```

GTK 3 reports a wheel step right as discrete `left` while its smooth delta
is `dx=+1`; the protocol trace shows `axis_discrete(1, 1)` and `axis(1, 10)`,
rightwards, so that is GTK's own horizontal discrete mapping.
The test client's button handler fires twice per Wayland button event; the
protocol trace shows one.

### Still open on GNOME 40

- GPU import (`wlp-import`): no DMA-BUF exists here; the host-memory path's
  rate and copy cost are measured (see Wayland Auto and Speed).
- Serving a Deck is done (see [Serving a Deck](#serving-a-deck-wlp-wiring));
  it was proven end to end on the Rocky 10 host, not on this one.
- A Deck-drawn (local) cursor end to end: the metadata dialect is the same
  as on GNOME 49 (Mutter 40 `meta-screen-cast-monitor-stream-src.c`, and
  the empty-sprite bytes above), but no Deck has been served one here.
- Multi-monitor, resize and output generations.
- Release packaging.
- Start-time selection requires `render_node_access`, which a host with no
  DRM device (where Mutter still composites on NVIDIA) never has.

## Host qualification (`wlp-wiring`)

Code: the pure record and validity rules in
`arcen_session::wayland_qualification`; the file and stack identity in
`hosts/linux/src/session/wayland/qualification.rs`; the command in
`hosts/linux/src/session/wayland/qualify.rs`. ADR 0014 Amendment 4.

**The record** (`/var/lib/arcen/wayland-qualification.json`, schema 3,
root-owned 0644 in a root-only directory, written atomically; a development
Pier names its own with `capture.wayland_qualification_record`, see
[Development records](#development-records)):

| Field | Meaning |
| --- | --- |
| `schema_version`, `qualified_at_unix_s` | schema and when the run finished |
| `host.pier` | `version`, `revision` (`ARCEN_SOURCE_REVISION` stamp or `unknown`), `executable_sha256` of the fused Pier |
| `host.stack` | `compositor` (`gnome-shell`/`mutter`) and its `compositor_version`, `pipewire_version` (linked library), `gpu_driver` (`vendor`, `version`, `drm`: whether `nvidia_drm` is loaded, `modeset`: its `modeset` parameter) |
| `os` | `id`, `version_id`, `kernel` — for operators only |
| `facts.capture_route` | route (`host-copy`, `gpu-convert`, `zero-copy-import`), first frame format, memory, size, and the access units encoded on it |
| `facts.encoder` | optional section: `native`, whether a native encoder (NVENC) opened for the session's stream (`available`/`missing`), and `capture_route_encoder`, the `READY` backend that encoded the route's proof. `native: missing` makes every session on the host a software session; an absent section means a native encoder is tried first |
| `facts.input` | backend, keyboard/pointer/touch/pen/EIS facts from the injector's `hello`, and pointer motions the session accepted |
| `facts.source_rates[]` | per measurement `width`, `height`, `display_refresh_hz` (the Deck display), `refresh_hz` (the output the session ran at), `codec`, `route`: the best rate both delivered and encoded over three consecutive delivering seconds, the measuring window, and `evidence` (`{"kind": "measured"}`; a record never carries an inference as evidence) |
| `facts.multi_output` | absent unless measured (`--multi-output N`, default 4 then 2, at the operator's first `--output`, else 1920x1080 at 60 Hz; an automatic re-qualification measures it too while within its time bound, else defers it to the next qualification): concurrent runs, each with every output's `width`, `height`, output `refresh_hz`, sustained source and encoded rates, the route and codec; a run in which every output encoded proves that many concurrent outputs (`QualifiedHost::max_concurrent_outputs`) |
| `facts.audio_monitor` | the session's PulseAudio has a monitor source |
| `facts.grading`, `facts.hdr` | `available` only when the fidelity session proved the preset on the stack's wide composition route; otherwise `missing` |
| `facts.fidelity` | optional: the route (`force-hdr`), or the gap's stable token and reason (`compositor_lacks_hdr_screencast`, `wide_formats_not_offered`, `wide_import_unproven`, `upconverted_eight_bit_content`, `wide_precision_unproven`, `reference_white_unproven`, …); per format and memory the ramp proof (expected and distinct codes, maximum error, native-wide); the measured SDR white in nits. Absent in records written before it |
| further `facts.<section>` | schema 3 is open to one optional section per package that proves more (multi-output, ten-bit/HDR proofs): `#[serde(default, skip_serializing_if = "Option::is_none")]`, absent means not proven; a section this build does not know is ignored (it proves nothing here), so a newer build's record reads as stale, not malformed, and is re-qualified |

**Validity.** The record is stale, and Wayland refused, when any of these
differ from the running host: Pier version, revision or executable digest;
compositor or its version; PipeWire version; GPU driver version, DRM
module presence or its `modeset` (which decides whether the compositor gets
a GBM/KMS device, and so its renderer, buffers and rate). A record of
another schema is refused, never trusted. A host-copy route proves the capture route, never GPU
import. A fact the live probe proves missing is never hidden by the record.

**Speed's rate.** A Wayland Speed request is decided by the shared
`arcen_session::wayland_qualification::resolve_speed` from the record's
measurements for the session's codec and capture route (`gpu-convert` once
verified, else the host copy), at the lowest output refresh that holds
Speed's floor (95 % of its ceiling, 57 fps for 60, `speed_minimum_fps`):
first a **measured** rate for exactly that size, else a rate **inferred**
from the smallest qualifying measured output that is at least as wide and
at least as tall, at the same refresh, codec and route — never upward,
never across refresh, codec or route. An inference is reported as such
everywhere: the Pier's log ("Wayland Speed rate is inferred …"), capenc's
`capture-rate-evidence=inferred:<W>x<H>` and its `ADMISSION … rate_evidence=`
line. A rate that reaches the floor admits the contract's own ceiling (the
floor exists so jitter in a source that holds the rate does not disqualify
it, so it does not clamp the served rate either). Anything else is served as
Auto (`copy_path_not_rate_qualified`) and its display is noted in the
**requested-displays list** (`<record stem>-speed-requests.json`, root-owned,
written only by the Pier, sizes only, at most 16) for the next qualification.
The measurement is capenc's own stats (one line per second, published by the
Pier's capenc supervision as `PipelineStatsSample`): each second counts the
frames both delivered (`source_fps`) and encoded (`enc_fps`), so an encoder
that cannot keep up caps the rate. The measuring capenc is admitted at
Speed's ceiling (only what it measured is recorded) on the route the product
takes (`gpu-convert` once verified), the Activities overview the first login
opens is left with Escape first, and the session's pointer moves at Speed's
rate (60 Hz) through the session input socket with the cursor embedded. The
rate is that of the compositor's repaint under pointer motion through
Arcen's own input path, not of an arbitrary workload; the runtime guard
([Speed without operator sizes](#speed-without-operator-sizes-wp-speed))
watches every served Speed epoch on the host's own counts.

**Which displays.** A qualification measures, each once and at most 10
(`qualification_outputs`): operator `--output`s, the default ladder
1920x1080, 2560x1600 and 3840x2160 at 60 Hz (`SPEED_QUALIFICATION_LADDER`),
up to 4 displays from the **requested-displays list** (most recent first, so
Decks in use are never crowded out), the previous record's displays worth
measuring again (of any schema, sizes only: one that held Speed and that no
larger qualifying output of the same refresh, codec and route covers —
`outputs_worth_remeasuring`; one that only failed is not carried, and comes
back if a Deck asks for it again), then the rest of the requested list.
Displays beyond the limit stay in the requested list for the next run.
`--replace-outputs` forgets the previous and requested ones. So the plain
command never loses a size that matters, and the installer's and the
Pier's hints need no size flags. Each display is measured at its own refresh
first; only codecs (`--codec`, default `h265` and `h264`) that do not hold
Speed there are measured again at the paced refresh in a second session.
Measured displays leave the requested list. **This host's own "no" wins:**
at a given refresh, codec and route, a failed measurement of the requested
size, of any size it contains, or of any size between it and the anchor
refuses inference (`resolve_speed`).

**The command.** `arcen-pier wayland qualify [--write] [--user NAME
--password-file FILE] [--output WxH@HZ]... [--seconds N] [--config PATH |
--no-config] [--verbose]`, root only, refused while `arcen-pier.service` is
active (a qualification session would compete for the encoder). It reads
the Pier configuration for the PAM service and session settings, then per
output: authenticates through PAM and the privileged launcher, opens the
headless session at that size, records readiness steps 1–4, proves input
through the Pier's own `WaylandInjector`, runs capenc's Auto composition
through `media::capenc::spawn` (the `ADMISSION` first frame proves the
route; access units must keep coming while the pointer moves), counts
pointer motions the session accepted on its socket, measures Speed's source
rate, checks the PulseAudio monitor (first output only), shuts the session
down and proves the logind session and runtime directory are gone. Before
the capture runs it asks capenc's `probe-v1` whether a native encoder opens
for the stream: without one it skips the GPU-import verification and the
Speed rate (software serves Speed as Software) and proves the capture route
with OpenH264, so an Intel or AMD host qualifies. It prints one line per step, what the record supports, whether this host can
serve Wayland and what start-up selection will decide under the configured
preference, and the record; `--write` stores it (a failed qualification is
recorded too, so the Pier refuses with its reasons). Exit status 0 only
when Wayland is servable with this record, teardown was clean and the
account was removed; 1 when not; 2 for a usage or precondition error. The
installer runs it on a host without Xorg once its static facts pass
(`arcen_session::install_provider`: compositor, logind, `dbus-daemon`,
PipeWire, WirePlumber, an accessible render node), starts the service only
when it exits 0, and otherwise leaves the Pier installed and stopped with
the failing lines; uninstall removes the record and a leftover `arcen-wlq`.

**Who the session runs as.** By default a temporary local account,
`arcen-wlq` (GECOS `Arcen Wayland qualification (temporary)`), created with
`useradd --create-home --shell /sbin/nologin` and a 32-character random
password set through `chpasswd`'s standard input, used for the run, and
removed with `loginctl terminate-user` and `userdel --remove`; a leftover
from an interrupted run is recognised by its GECOS and removed first, and an
`arcen-wlq` the command did not create is refused. This was chosen over
borrowing an operator's account because it touches no real user's home,
settings or desktop, needs no directory (AD/LDAP) account, and leaves
nothing behind; the password exists only in the process and the shadow
file for the length of the run and cannot be used for an interactive
login. `--user NAME --password-file FILE` uses an operator-named account
instead, which must be in `/etc/passwd` with a uid of 1000 or more and must
not be logged in.

### Development records

`capture.wayland_qualification_record` (Pier configuration) or `qualify
--record PATH` names a record other than the production one
(`arcen_session::wayland_qualification::QualificationTarget`). A run for it
uses its own temporary account, `arcen-wlq-<first 8 hex of the path's
SHA-256>` (or `--temporary-user arcen…`), whose GECOS names that digest, and
its own transient scope (`arcen-wayland-qualify-<hex>`); it never creates,
uses or removes `arcen-wlq`. `--allow-running-service` lets it start beside
an active `arcen-pier.service` (encoder contention is the operator's call) and
is refused for the production record. The same root-only rules apply to the
record and its directory.

### Automatic re-qualification at start

A routine update of the compositor, PipeWire or GPU driver, or a Pier
upgrade, makes the record stale. When start-up selection refuses Wayland
*only* because the record is stale or of another schema (no live blocker:
compositor, logind, PipeWire and the render node all present), the Pier
service re-qualifies the host itself before it listens (shared
`auto_requalify_decision`, `wayland::requalify_at_start`): the same
qualification code, as root, outside any login session, with its record's
temporary account, at most 5 displays (others go to the requested list) and
no new display after 10 minutes, 6 s of motion per Speed measurement. Each
step is logged (`arcen::session`, "automatic Wayland re-qualification: …").
It never runs for a missing record (the installer qualifies a new host) or
one that is unreadable, not root-only or malformed (refused as before). The
attempt is recorded in `<record stem>-requalify.json` (stack digest, time,
outcome, failing steps); a failed attempt on the same stack is not repeated
for 6 hours, so `Restart=on-failure` cannot loop it, and start-up refuses
with the failing steps and the command. The marker is written as started
before the run: a start that still finds it running knows the attempt was
interrupted (crash, kill) and retries at once, at most
`AUTO_REQUALIFY_MAX_INTERRUPTED` (3) times in a row on one stack. The
service's signal handlers exist before the run (`service_signals`): SIGHUP
only reopens the log, and SIGTERM abandons the desktop, removes the
temporary account and the marker, and exits 0. A killed run's logind user
can stay `closing` (systemd 257) until logind restarts; the account is
removed anyway and the next one takes a UID nothing holds
(`temporary_account_uid`), so no logind restart is ever needed. `capture.wayland_auto_requalify =
false` restores refuse-only start-up. Proven on Rocky 10.2 / GNOME 49.4 in
the service's own SELinux domain (`unconfined_service_t`, development record):
a rebuilt Pier (executable digest changed) re-qualified five displays (four
from the previous record, one a Deck had requested) at start and listened
after about 140 s; with a PAM service that rejects the temporary account the
attempt failed, wrote no record and its marker, and the next start refused in
about 2 s with the failing steps, without re-running.

**Where it runs.** `pam_systemd` opens no logind session for a caller that
already has one, so a command started from an operator's SSH or console
shell re-executes itself through `systemd-run --scope` (unit
`arcen-wayland-qualify`): outside the login session, still in the
operator's own SELinux context, which can enter the session user's
context with or without Arcen's SELinux module. A Pier run as a systemd
service (`unconfined_service_t`) can do so only with the module loaded;
see [Linux Pier under SELinux](../security/linux-selinux.md).

## Serving a Deck (`wlp-wiring`)

The Pier's network path serves a headless Wayland session with the same
attachment code as Xorg; what differs is decided from the session's own
facts:

- **Session.** On an authenticated attachment the launcher opens the
  headless session at the served Deck display's size
  (`SessionDisplayMsg`); a reattach within the reconnect window reuses it.
  On a host whose sessions are all served in software (an operator's
  `software-h264` pin, or `auto` with a record proving no native encoder,
  `Config::wayland_software_sessions`) that size is first fitted to
  OpenH264's limits with the shared resolver (`wayland_session_size`,
  logged `Wayland session output fitted to the software encoder's limits`):
  a Wayland output is created once, before any encoder exists.
  The attachment takes the stream's size from the session and never runs
  the X MetaMode resize (there is no X screen); the hello therefore offers
  no `display_update`.
- **Video.** capenc gets the session's hand-off and, for Speed, the
  record's rate for that stream size (`capture-rate=`, Wayland only).
  A software host asks capenc for `encoder=software-h264` directly
  (`Config::session_encoder`); elsewhere the usual ladder runs (NVENC, other
  NVENC codecs, then OpenH264), and its software step follows the stream's
  cursor truth and refuses, by name, an output larger than 1920x1200 that
  can no longer be fitted (see [Software encode](#software-encode-linux-wayland-software)). The
  Pier waits for `ADMISSION` and `READY`; the hello's `active_pipeline`
  comes from the admitted plan, so a Speed request served as Auto is
  announced as Auto. A Grading or HDR request the record does not prove is
  resolved before capenc starts (`Config::wayland_fidelity_fallback`): HDR
  to Grading when only Grading is proven, otherwise to Auto, with the
  record's gap as the reason (`eight_bit_source` and
  `hdr_on_eight_bit_source` when the record has none), logged with the
  requested and served variants; a later quality request outside what was
  admitted keeps the admitted plan instead of respawning into a refusal.
  A proven preset is served on its route; see
  [Wayland Grading and HDR](#wayland-grading-and-hdr-wp-fidelity).
- **Input** goes through the session's injector (see [Input](#input)).
- **Audio** is audiocap recording the session sink's monitor through the
  session's PulseAudio-protocol server (see [Audio](#audio-wp-audio)); the
  **Deck microphone** is the existing PulseAudio pipe source, loaded into that
  same server, so native PipeWire applications see it too (fixed earlier for
  every Linux session: the agent duplicated rather than reopened its
  root-owned input pipe).
- **Resize during a session** (remote-monitor sessions, one output,
  Windowed Decks; on the software encoder the requested size is first
  fitted to OpenH264's limits and the fitted size is what the Deck is
  answered, and an attach fits an output an earlier epoch left larger): a
  `display_update` retires the capture generation,
  configures the agent's output to the new size as the next output
  generation, starts a new capenc epoch on the new hand-off, waits for its
  keyframe, moves the session-bound injector to the new generation and
  answers `display_update_result`. A refused transaction keeps the previous
  output and answers with the reason. Every attachment reads the live
  outputs from the agent, so a reattachment after a resize captures the
  resized output.
- **Recovery inside an attachment.** A capture epoch that ends on its own
  (capenc died, its stream was replaced) is replaced, keyframe first,
  without ending the attachment; audio, microphone and input keep running
  (`arcen_media::video::CaptureRestartBudget`, three restarts per minute,
  then the Deck is told `video capture keeps failing on the host`). Every
  try, including one that fails to start or to deliver a keyframe (an
  agent that has not yet replaced a closed stream), consumes a restart and
  is retried a second later on the agent's current outputs. A
  desktop whose agent is gone (compositor or stack death) ends the
  attachment with `the desktop session on the host ended`; the next
  attachment starts a new desktop.
- **Reattach.** A sign-in that reattaches to a remote-monitor desktop
  whose outputs differ from the ones it asks for (another display size,
  layout or count) moves the desktop to them in one output transaction
  before its first capture epoch; the desktop and its applications stay. A
  refused transaction keeps the live outputs. A fixed-output desktop with
  another topology is replaced, as on Xorg.
- **Clipboard** both ways, text and PNG, at the same policy as Xorg, through
  the session's own bridge (see [Clipboard](#clipboard)).
- **Unavailable in a Wayland session**, stated rather than attempted:
  resize in a fixed-output (startup-monitor) session or a non-Windowed
  Deck, pen/tablet, Grading and HDR. Hard USB depends on the build, not the provider
  (not in this build).
- **Capacity one** is unchanged: the admission gate refuses a second
  attachment while one is live ("shared display is busy").
- **Teardown.** Logout (`loginctl terminate-session`) and Pier shutdown run
  the session's release; a detached desktop otherwise persists like an Xorg
  one (`platform.session.disconnected_idle_timeout_secs`).
- **X11 applications** run through the compositor's Xwayland where the
  host's qualification proves it (`platform.session.wayland_x11_apps`; see
  [X11 applications](#x11-applications-xwayland)).
- **Not implemented yet** (each a tracked work package, not a limit of the
  stack): vendor-neutral
  hardware encode on Intel/AMD (VA-API; such a host is served by OpenH264,
  see [Software encode](#software-encode-linux-wayland-software)); resizing the output to a
  reattaching Deck's display (a reattachment keeps the session's size);
  Unicode text beyond the session's keyboard layout.

### Desktop behaviour inside a headless session

The desktop runs `gnome-shell --headless` inside the GNOME login the agent
assembles ([GNOME login services](#gnome-login-services-wp-gnomesession)).
The dedicated Xorg desktop still runs `gnome-shell --x11 --sm-disable` without
any of it, apart from the login-shell environment and the host-sleep
inhibitor. What follows is measured on Rocky Linux 10.2 / gnome-shell 49.4 /
gnome-settings-daemon 47.2 / gnome-keyring 42.1 / GDM 47. The runs were on
2026-10-18 with a throwaway local user through the real launcher.

| Behaviour | Result | Why |
| --- | --- | --- |
| Session manager | `org.gnome.SessionManager` owned by the agent; `SessionIsActive=false`, `CanShutdown=false`; 15 clients registered (the settings daemons, the shell, the portal) | See [GNOME login services](#gnome-login-services-wp-gnomesession) |
| Settings daemons | `gsd-a11y-settings`, `-color`, `-housekeeping`, `-keyboard`, `-media-keys`, `-power`, `-print-notifications`, `-screensaver-proxy`, `-smartcard`, `-sound`, `-wacom`, `-disk-utility-notify` and `-printer` running in the session scope as `unconfined_t`. `-datetime`, `-rfkill`, `-sharing`, `-usb-protection`, `-wwan`, `spice-vdagent` and `vmware-user` not started (host-scoped). `-xsettings` waits for an X11 display | Policy table in `arcen_session::desktop_session::components` |
| Autostart | A `~/.config/autostart` probe ran with `WAYLAND_DISPLAY=wayland-0`, the login-shell environment and `SSH_AUTH_SOCK`, in `session-<id>.scope`. GNOME Software, Tracker, the Evolution alarm, SELinux troubleshooter and xdg-user-dirs ran as on a GDM login. Orca and the accessibility-bus entry were skipped (their `GSettings` conditions are false) | |
| Keyring | One `gnome-keyring-daemon --unlock` owns `org.freedesktop.secrets`. The login collection is `Locked=false`; `secret-tool store` and `lookup` work without a prompt, and the value stored in one desktop is read in the next. `ssh-add -l` reaches its agent. The password appears in no process's argv or environment (0 matches across the session, agent and launcher) | The launcher hands the password over a pipe |
| Idle blanking, idle lock | With `idle-delay` 30 s, `lock-delay` 0: presence went `Idle` (3), `ScreenSaver.GetActive` true and `LockedHint=yes`. The password typed through the session's input socket unlocked it (presence back to 0). An idle inhibitor (`gnome-session-inhibit --inhibit idle`, `InhibitedActions=8`) kept presence available for 45 s | Presence follows Mutter's `IdleMonitor`; RemoteDesktop input resets it |
| Suspend and blanking by `gsd-power` | None. The overlay pins `sleep-inactive-ac-type='nothing'` (not writable). An extra `gsd-power` started with the user's own `sleep-inactive-ac-type='logout'` and a 60 s timeout (overlay removed) logged `inactive, so using normal state` and `ignoring state transition to blank as inactive`; after 95 s the desktop was still there, `PowerSaveMode=0`, logind `IdleHint=no`. logind lists Arcen's `sleep:idle` block inhibitor for the desktop's life, and only delay inhibitors from the shell and the settings daemons (`gsd-media-keys` does not get its power-key inhibitor: polkit refuses it to a session without a seat, and Arcen's polkit rule keeps a native desktop's session to the same `allow_any` answers) | `SessionIsActive=false` is gsd-power's own gate; the launcher holds the inhibitor |
| Idle lock and reattachment (2026-10-19, real Deck GUI on a Mac, dev Pier 7e6dbb42) | Throwaway user with `idle-delay` 30: attached without input, presence `Idle` at 30 s, `LockedHint=yes` at 40 s; a reattachment unlocked it (`LockedHint=no`, `GetActive` false, presence back to 0) and it stayed unlocked for 60 s without input; after input it locked again 30 s later; 2 `AddIdleWatch` calls in the whole run. Owner's AD account (`idle-delay` 300): locked (stream black) after 300 s attached without input, unlocked by the next reattachment | Presence follows the shield and keeps one persistent idle watch (see the session manager above) |
| Manual lock (Super+L) and unlock | Super+L through the input socket locks (`GetActive` true, `LockedHint=yes`, handled by `gsd-media-keys`); `loginctl unlock-session` (what the Pier runs on every password-authenticated attachment) unlocks | GNOME Shell unlocks through GDM's reauthentication channel and deactivates on logind `Unlock`. Without GDM the shell creates no screen shield at all |
| Time-zone redirection | Opened with `America/New_York` on a host in BST: the top-bar clock read 02:44 while the host read 07:44 | `TZ` is in gnome-shell's environment and in every service the private bus activates |
| First login | `gnome-initial-setup --existing-user` runs (autostart) and exits at once on Rocky Linux 10: "Skipping gnome-initial-setup for existing user" (the distribution's `vendor.conf` limits existing users to language and keyboard pages, which do not apply here). The shell's welcome dialog opens as before. With `wayland.first_login_setup = "skip"` both markers are written first | GNOME's own behaviour; nothing is suppressed by default |
| Keyring with another password | After the account password was changed without the keyring: `failed to unlock login keyring on startup`, `Locked=true`, logged `keyring locked`; the desktop runs and applications ask for the keyring password | As after a GDM login |
| Log Out | The shell's dialog opens; Return activates its default button, Cancel (logged `logout cancelled`). Left alone, its 60 s countdown confirms: all 15 clients answered `EndSession`, the agent ended the desktop (`user_logout`) 65 s after the request, and the launcher released the scope, runtime directory and inhibitor | |
| `wayland.first_login_setup = "skip"` | `~/.config/gnome-initial-setup-done` written (`yes`), `welcome-dialog-last-shown-version` raised to `49.4`, no initial-setup process | |
| Start time | Open-to-ready through the launcher, three alternating runs: the installed 0.17.0 launcher 1.56–1.80 s, this build 2.04–2.11 s (the overlay compile, keyring start and login shell run before the compositor; the settings daemons and autostart start after it and are not waited for) | |
| Notifications and portals | GNOME Shell's notification service owns `org.freedesktop.Notifications` and `notify-send` works; the settings portal answers (`color-scheme`) | |

### Lab evidence (2026-10-17, Rocky 10.2 / GNOME 49 / NVIDIA GRID, DRM)

Rocky Linux 10.2 (kernel 6.12), gnome-shell 49.4, PipeWire 1.4.11, GRID
V100D-16Q driver 580.105.08 with `nvidia-drm modeset=1`, Secure Boot and
SELinux enforcing, no Xorg server installed; a debug Pier built from this
branch, run as a transient systemd unit with
`SELinuxContext=unconfined_u:unconfined_r:unconfined_t` (before the
[SELinux fix](../security/linux-selinux.md)), a throwaway local account, the macOS Deck's `media-smoke`
and `input-smoke` over a VPN:

| Step | Evidence |
| --- | --- |
| Qualification | `qualify --write --output 1920x1080@60 --output 1800x1130@60` as `arcen-wlq`: route `host-copy`, `bgrx8` `host-shared`, ~90 access units in 3 s; input `hello` device types 7, 121 pointer motions accepted in 1 s, `eis_offered=true`; source rate 40–41 fps sustained at both sizes (Speed not qualified); PulseAudio `auto_null.monitor`; logind session, runtime directory and account removed |
| Selection | `auto`, no record: refused naming `qualify --write`; `auto` with an Xorg executable configured: X11 (`xorg_server_installed`), record not read; `auto` with the record: Wayland (`wayland_qualified`) |
| Renderer | the headless gnome-shell is a graphics process on the NVIDIA GPU (`nvidia-smi`, 110 MiB) with `libnvidia-eglcore` mapped, `/dev/nvidia0` and `renderD128` open; ScreenCast still delivers shared memory, encoded by host copy |
| Auto | PAM, session at the Deck's 1800×1130 in ~1.6 s; `ADMISSION admit auto 30 fps host-copy`; hello HEVC 4:2:0 8-bit limited BT.709, `active_pipeline=auto`; VideoToolbox decoded |
| Speed request | `downgrade … copy_path_not_rate_qualified served=auto`; hello `active_pipeline=auto` |
| Grading / HDR request | `downgrade` `eight_bit_source` / `hdr_on_eight_bit_source`, served `hevc-420-8-limited-bt709`; the Deck's 4:4:4 quality request kept the admitted plan; decoded |
| Audio | Opus frames from the session's PulseAudio monitor every run |
| Microphone | negotiated Opus, 36–39 Deck frames accepted into the session's pipe source |
| Input | `input-smoke`: 3 keys, 1 motion, buttons and scroll injected (`key_events=3 mouse_moves=1`), injector proven per attachment |
| Reattach | every later attachment reattached to the same logind session |
| Keyframe on demand | 27 ms on a still desktop (harness, same build and stack) |
| Teardown | logout: logind session, runtime directory and mounts gone, user manager stopped ~10 s later; Pier stop: the same |

Earlier the same day, with 580.105.08 and `nvidia-drm` loaded at
`modeset=N`, the same qualification and Deck runs (Auto, Speed served as
Auto, input, reattach, logout) passed. The no-DRM refusal is covered by
unit tests only: this host was not returned to a driver without DRM.


### Lab evidence: remote monitors, resize and recovery (2026-10-17, same host)

Rocky Linux 10.2, gnome-shell 49.4, PipeWire 1.4.11, GRID 580.105.08 with
`nvidia-drm modeset=1`, debug Pier from `wlp/recovery` as a transient
unit, macOS Deck `media-smoke`/`input-smoke` over a VPN:

| Step | Evidence |
| --- | --- |
| `RecordVirtual` behaviour | a consumer fixing 1280×800 created `Meta-1` "Virtual remote monitor" 1280×800; it stayed after the consumer left; a consumer fixing 1600×900 resized it in place; `max-framerate=120/1` gave a 120 Hz mode; two streams recorded before `Start` gave two monitors, `ApplyMonitorsConfig` placed them; a stream recorded after `Start` announced no node until it was started with `ScreenCast.Stream.Start` (found 2026-10-18, see [Multi-monitor](#multi-monitor-wp-multimon)); stopping the session removed its monitors; absolute pointer motion on a virtual stream was accepted |
| Qualification | remote mode, session ready in 2.0 s; capture, input and audio as before; source rate sustained **60 fps** at 1920×1080 and 1280×800 (host copy), against 40–41 fps with `--virtual-monitor` |
| Resize | Windowed Deck, `ARCEN_MEDIA_SMOKE_RESIZE=1600x900,1280x720,1800x1130`: each accepted in about 0.6 s (generations 2, 3, 4), decoded at each size, audio packets 194 → 279 → 520 → 726 across the three resizes; `input-smoke` after the resizes injected 3 keys, motion, buttons and scroll |
| Topology change | `configure` (root, on the output socket) generation 2 with 1600×900 + 1280×1024 at (1600, 0): `Meta-1`, `Meta-2` placed side by side in 0.4 s; back to one 1800×1130 output as generation 3; a stale generation was refused |
| capenc killed | replacement epoch streaming with a keyframe 0.38 s later; the smoke completed (984 audio packets in 22 s) |
| gnome-shell killed | the Deck was told `the desktop session on the host ended` 0.5 s later; the next attachment started a new desktop |
| Deck killed | (transport) a same-user reattachment proved the old Deck silent in 3.0 s and streamed; before, it was refused `shared display is busy` |
| Deck killed, resume negotiated | (after review, with `feat/wayland-provider` 9e6703a1 merged) the displaced attachment handed the desktop over 0.3 s after the 3.0 s proof; the newcomer reattached to the same logind session and streamed |
| Resize during input | pointer motion every 50 ms across three resizes: one motion refused as `stale output generation` at a switch, the injector stayed connected |
| Speed qualification | `qualify --output 1920x1080@120` with remote monitors and the GPU route (`gpu-convert`, DMA-BUF, explicit sync): Speed admitted, source 59–60 fps every second, sustained 60, encoded 60–61 |

Not injected: Mutter closing the paired session while the compositor
runs (no client other than the agent may stop it), PipeWire daemon death
(a stack member: the session ends), GPU-import failure (the import route
belongs to `wlp-gpu-speed`; a capenc that exits on it is restarted by the
same budget).

## Audio (`wp-audio`)

Code: `hosts/linux/src/session/wayland/audio.rs` (mode, drop-in, lease),
`hosts/linux/src/session/wayland/stack.rs` (members),
`hosts/linux/src/media/local_playback.rs` (`pactl` lease adapter),
`hosts/linux/src/media/pipewire_bridge.rs` (pulse tunnels), and the pure
policy in `arcen_session::local_playback`.

A Wayland session's applications inherit the compositor's private
`XDG_RUNTIME_DIR`, so native PipeWire clients reach the session's private
`PipeWire`, never the user's. That daemon therefore carries the session's
audio itself. `resolve_wayland_audio` chooses how, from host facts (never
the distribution's name):

| Mode | When | What the session gets |
| --- | --- | --- |
| `private_graph` | `PipeWire`'s PulseAudio-protocol module and `pipewire-pulse.conf` are installed | A session sink (`arcen_session_sink`, `support.null-audio-sink`) created by the private daemon from a drop-in in a private `XDG_CONFIG_HOME` (which adds to the system configuration; `PIPEWIRE_CONFIG_DIR` would replace it; the user's own `~/.config/pipewire` files are linked in). A private `pipewire -c pipewire-pulse.conf` serves PulseAudio clients on the same graph at `<runtime>/pulse`, and every member, the compositor and its applications get `PULSE_RUNTIME_PATH` pointing there. |
| `bridged_to_user_pulse` | No PulseAudio-protocol module, but `PipeWire`'s pulse tunnel (GNOME 40 class hosts with PulseAudio 15) | A pulse tunnel in the private daemon's drop-in: a sink named `arcen_session_sink` that plays into the session sink of the user's PulseAudio. The microphone is a second tunnel, `arcen_session_microphone`, run by the agent as its own `pipewire -c` client of the private daemon and recording the user's default source: `PipeWire` connects a tunnel's recording stream so that PulseAudio cannot move it, so the agent restarts that client on the new default whenever the default source changes (each Deck microphone generation), and never records a sink's monitor. The tunnels reconnect only where `PipeWire` supports it (1.1.0 and later). |
| `user_pulse_only` | Neither | PulseAudio clients only, logged as `native_pipewire_unserved`. |

The private `pipewire-pulse` is optional: a failed start or an exit is
logged and the member is restarted (three times a minute), and it never
ends the desktop or video. The session sink lives in the daemon, so it
survives a restart; a microphone source loaded into the server does not,
and the microphone path reports that to the Deck as it always has.
WirePlumber links native and PulseAudio streams to the session sink, the
only sink, and keeps its state (`XDG_STATE_HOME`) in the private runtime
directory, never in the user's `~/.local/state/wireplumber`.

audiocap records the session sink's monitor (`--source`), through the
session's PulseAudio-protocol server; the microphone agent loads the Deck
microphone's pipe source into the same server. The agent reports both in
`AgentReady.audio`, and the launcher points every helper's environment at
them.

**Local playback.** With `audio.local_playback = muted` the agent holds a
lease on the session's server for the whole session (see
[Pier configuration](../operations/pier-configuration.md#local-playback)).
In a private graph nothing can reach the room, and the same check proves
it (`basis = private_graph`); in a bridged graph the lease runs on the
user's PulseAudio, as on X11. With `audible`, a private graph's session sink
is also looped into a tunnel to the user's own default sink.

### Lab evidence (2026-10-18)

Host-side, through the real launcher and session agent of a development
build (a temporary local account; the installed Pier untouched), Arcen's
own `audiocap` recording the session's capture source while a 440 Hz test
tone (amplitude 8000, RMS about 5660) played:

| Host | Run | Evidence |
| --- | --- | --- |
| Rocky 10.2, GNOME 49.4, PipeWire 1.4.11, WirePlumber 0.5.10 (`private_graph`) | Wayland, `muted` | gnome-shell's environment carries `PULSE_RUNTIME_PATH=<runtime>/pulse`; the private server reports `PulseAudio (on PipeWire 1.4.11)` with `arcen_session_sink` the only sink; native `pw-play` linked to `arcen_session_sink:playback_FL/FR` by WirePlumber, `audiocap` RMS 4692 over 418 560 samples; PulseAudio `paplay` the same, RMS 4692; a microphone pipe source loaded into the session server appears as an `Audio/Source` node of the private graph, native `pw-record` RMS 4821 and PulseAudio `parecord` RMS 5477 from it; local playback `basis=private_graph`, honoured; the user's `~/.local/state/wireplumber` unchanged, the private one written; after teardown no session, runtime directory or `pipewire-pulse` left |
| Rocky 9.7, GNOME 40, PulseAudio 15, PipeWire 1.0.1 (`bridged_to_user_pulse`) | Wayland, `muted` | the user's PulseAudio default is the lease's `arcen_session_<tag>`; the private graph has `arcen_session_sink`; native `pw-play` linked to it, `audiocap` on the lease sink's monitor RMS 5168; PulseAudio `paplay` RMS 4606; no microphone node until a pipe source becomes the default, then `arcen_session_microphone` records it, native RMS 4635 |
| same | Wayland, `audible` | the tunnel plays into the user's default sink: native RMS 5178, PulseAudio RMS 4606, microphone RMS 4635 |
| Rocky 9.7, GNOME 40, Xorg, PulseAudio 15 | X11, `muted` | at start the session sink is the default; a "speaker" sink loaded and made the default mid-session is muted, the default put back and a stream played straight to the speaker moved onto the session sink within 1.5 s; unmuting the speaker by hand is undone within 1 s; `audiocap` RMS 5601; native `pw-play` (before this work: "no node available", the user's PipeWire had no audio sink) RMS 5556 through the bridge; the bridged microphone RMS 4635; after the session the speaker is unmuted and no Arcen module or journal remains |
| same | X11, agent killed with SIGKILL | the journal named the muted speaker and the loaded module; the launcher's recovery unmuted the speaker, removed the module and the journal |
| Rocky 10.2 (as above), release development Pier on its own port, macOS Deck `media-smoke --microphone` for 61 s | Wayland, `muted`, end to end | Pier: `native audiocap started source=arcen_session_sink.monitor`, `audio_frames_captured=2899`, `audio_frames_sent=2878`, none dropped; while the Deck was attached, native `pw-play` and PulseAudio `paplay` (amplitude 6000) linked to `arcen_session_sink` and the monitor that audiocap records measured RMS 3988 and 3999; the Deck microphone source `arcen_microphone_<generation>` was the session server's default and an `Audio/Source` node of the private graph, 2033 Deck frames accepted, a native `pw-record` and a PulseAudio `parecord` both read it (the Deck's microphone was silent, RMS 0). Deck side, over the Norway→London VPN link: 2866 audio packets, peak 32767 |
| same | X11, `audible` | the user's audio server is not touched (the speaker stays the unmuted default); native and microphone bridges as above (RMS 5556, 4635) |

Two findings fixed on the way: a tunnel's PulseAudio stream cannot be moved
(`PA_STREAM_DONT_MOVE`), so the tunnels restart instead of moving; and a
lease must never mute a stream, because PulseAudio's stream-restore and
WirePlumber's `restore-stream` remember that mute for every later stream of
the same application (a muted tunnel left the next session's native audio
silent).

## Multi-monitor (`wp-multimon`)

A Wayland session serves Match My Layout with the same `multi_monitor_v1`
negotiation, carrier and fair monitor mux as a dedicated Xorg session; what
differs is where the monitors come from.

| Stage | Dedicated Xorg | Headless Wayland (remote monitors) |
| --- | --- | --- |
| What can be offered | NVIDIA `DFP-N` heads found by the start-up Xorg probe | the compositor's output mode (`VirtualOutputMode`, up to four) and the concurrent outputs the qualification record proved (`facts.multi_output`); no head discovery |
| Planning | `display::topology::plan_topology` onto heads | `display::topology::plan_virtual_topology`: the shared `arcen_outputs::virtual_topology` placement (already-oriented extents, desktop at the origin, refresh and millimetres kept), then Mutter's layout rule (`validate_layout`); refused whole otherwise |
| Rotation | per head | not offered: Mutter accepts a transform on a virtual monitor (lab, 2026-10-18), but what its stream then carries is unproven |
| Encoder | NVENC or OpenH264 by policy and measured admission | the same aggregate admission; each output's capenc measures NVENC on synthetic frames at the output's size (`admission-v1`) without consuming its stream; `auto` resolves to the encoder the Wayland compositions encode with (`Config::wayland_multi_monitor_encoder`) |
| Desktop | multi-head Xorg from the plan | the launcher opens the plan as placed outputs, primary first; the agent's ready message must match it |
| Capture | one capenc per NvFBC output | one capenc per output, each with its own stream's hand-off, Speed rate and import route (`MonitorCaptureBinding`); a monitor without a matching stream refuses the attachment |
| Input | uinput over the combined raster | `pointer_absolute` on the output that shows the region, in its stream pixels and output generation |
| Applied capability | `DedicatedXorg`, refresh placeholder | `VirtualOutputs`, the monitor's refresh |
| Another layout at reattach | the desktop is replaced | the outputs move in place before the first capture epoch; the desktop stays |

Speed is not admitted per output: the record's Speed rates are
single-output ones, the runtime guard watches one capture, and
`facts.multi_output` measures Auto. Each output's capture serves a Speed
request as Auto (`copy_path_not_rate_qualified`; the Pier logs
`speed_multi_output_unqualified`), and the hello labels the set with what
every monitor serves, until a concurrent Speed proof exists.
Grading and HDR are not admitted on several outputs either: the record's
wide-composition proof (`facts.fidelity`) is a single-output one, so a
multi-monitor sign-in that asks for either is resolved as Auto, eight-bit
and without the wide composition route, before its desktop or encoders
exist (`fidelity_multi_output_unqualified`).

A monitor's capture that ends on its own ends the attachment, as on Xorg
(the mux tears every monitor down together); the Deck's resume reattaches.
On a startup-monitor compositor (below Mutter 49) the offer is withheld:
Mutter 40 accepts several `--virtual-monitor` flags and `ApplyMonitorsConfig`
does not exclude virtual monitors there (source, 40.0), but that is not yet
proven on a GNOME 40 host.

### Lab evidence (2026-10-18, Rocky 10.2 / GNOME 49.4 / NVIDIA GRID V100D; GNOME 40.9 baseline host)

All rates are host-side (capenc counters). Deck-side figures in these runs
crossed a long-haul VPN link and are not host facts.

- **Streams added to a running session (Mutter 49.4).** A `RecordVirtual`
  stream recorded after `RemoteDesktop.Start` announces its PipeWire node
  only after `ScreenCast.Stream.Start` on that stream; `Stream.Stop` removes
  its virtual monitor. Transforms and the mode's scale list (`[1, 2]` for
  1920x1080, `[1]` for 1280x720) are accepted on virtual monitors.
- **Qualification.** `arcen-pier wayland qualify --multi-output 4` proved
  four concurrent 1920x1080 outputs on the DMA-BUF `gpu-convert` route; each
  output encoded about 30 fps at Auto while idle and its source delivered
  58–60 fps while the qualification pointer visited it.
- **Match My Layout.** Deck fixtures of two, three and four displays were
  applied whole (desktops 3520x1080, 4800x1080 and a 2x2 3840x2160), every
  monitor decoded, and the region input probe's eight events were accepted
  on the output that shows each region.
- **Reattach in place.** Successive sign-ins moved one desktop 1→2→3→4 and
  2→1 outputs with the same `gnome-shell` process; each attach-time output
  transaction committed in 80–420 ms before the first capture epoch.
- **UI scale.** A 3024x1964 HiDPI stream at Automatic (the Deck advertises
  200%) is placed at scale 2.0 with an unscaled 1920x1080 neighbour at 1.0.
  The same display streamed in points (1512x982, 100%) stays at 1.0; its
  mode offers only `[1]`. For comparison, a 200% HiDPI request (3600x2262)
  to a single dedicated Xorg head on GNOME 40.9 produced a 480x300 mm EDID
  (190.5 DPI) that Mutter 40's `> 192 DPI` rule leaves at scale 1.0.
- **GNOME 40.** Two `--virtual-monitor` flags give `Meta-0` and `Meta-1`;
  `ApplyMonitorsConfig` accepts stacked, left-of and scale-2 layouts in the
  physical layout mode, and `RecordMonitor` per connector yields a node per
  output, but consuming frames from those nodes was not proven, so the offer
  stays withheld below Mutter 49.

## Running a newer GNOME than the distribution ships (`wp-gnomeupgrade`)

> **Lesson.** The headless session is Arcen's own process tree. The agent
> starts the compositor by path, gives it a private bus, a private PipeWire and
> a private runtime directory, and proves the result before any Deck sees it.
> The distribution's GNOME is therefore a default, not a limit.
>
> Sometimes the distribution is older than the capability we need:
>
> - HDR screen sharing arrived in Mutter 50.
> - Screen-cast pacing arrived in 51.
> - A headless input seat is not released yet.
>
> Then we neither wait for the distribution nor replace its packages. We
> build the compositor into a side-by-side prefix that only Arcen's sessions
> use, prove it with the same qualification code, and remove it with one
> `rm -rf` when the distribution catches up. "The lab runs GNOME 49" was a
> fact about one machine, not about what Arcen can do on it.

### Why a prefix, not packages

Enterprise Linux 10 has no newer GNOME to install:

- Rocky Linux 10.2 ships Mutter 49.4 and GNOME Shell 49.4.
- CentOS Stream 10 tops out at Mutter 49.4 and GNOME Shell 49.5, with libei
  1.5.0 and PipeWire 1.6.9 (checked 2026-10-18).
- EPEL may not replace RHEL packages.
- No EL10 COPR carries GNOME 50 or 51.

Forcing Fedora packages into `/usr` would replace glib, gjs, libinput,
PipeWire and the GSettings schemas for the whole system:

| What GNOME needs | What EL10 has |
| --- | --- |
| GNOME Shell 50/51: gio ≥ 2.86 and gjs ≥ 1.87.1 (SpiderMonkey 140) | glib 2.80, gjs 1.80 |
| Mutter 51: libinput ≥ 1.31, wayland-server ≥ 1.26, libpipewire ≥ 1.6, gsettings-desktop-schemas ≥ 51.rc | older versions of each |

### Mechanism

- **Pure policy (`arcen_session::compositor_prefix`).**
  - `CompositorPrefix` is absolute and normalised, with no `.`/`..`, `:` or
    whitespace, and is not `/`. The default is `/usr`.
  - It gives the binary (`<prefix>/bin/gnome-shell`), the data directory, and
    the session's `XDG_DATA_DIRS`: the prefix's `share` first, then the
    inherited list or the XDG default, without duplicates.
  - `verify_prefix` decides whether a root service may execute anything
    from it, and returns the compositors it holds. Every path must be
    root-owned and not group- or world-writable:
    - each component of the configured path and of its real path, so no
      user can swap a parent;
    - everything inside the prefix, recursively: every binary (not only the
      compositor that will be chosen), `lib64`, `libexec`, `share`, `etc`.
      Root loads all of it through RPATH when it probes or runs a binary;
    - every link, by its own owner and by its target. A target outside the
      prefix passes the same rule with its ancestors, and a directory target
      is walked too. A dangling link refuses the prefix.
  - The platform adapter only answers `lstat`, `readdir` and `realpath`
    (`PrefixInspector`). The Pier and the installer each have one.
  - A prefix must hold an executable `bin/gnome-shell` or `bin/mutter`.
- **Probes never need root.** When the Pier, `wayland qualify` or the
  installer runs as root, a prefix binary's `--help` or `--version` runs as
  `nobody`. The installer verifies the prefix before probing. A refused
  prefix stops the install with the reason and nothing in it is run.
  The support bundle reports `compositor_prefix_error` and a
  `compositor_prefix` blocker instead of evaluating `/usr` in its place.
- **One prefix per process.**
  - The Pier and `wayland qualify` read `capture.wayland_compositor_prefix`.
    They verify it and set it once (`session::wayland::apply_configured_compositor_prefix`)
    before provider selection, so probing, the qualification stack identity,
    output reconfiguration and the launch all use the same binary.
  - The launcher's open request carries the prefix, and the launcher (root)
    re-verifies it.
  - The agent receives `--compositor-prefix`.
  - A process never mixes two prefixes: a second, different value is
    refused.
- **The session.**
  - Only the members' `XDG_DATA_DIRS` changes. The private bus finds the
    prefix's D-Bus services, GSettings finds its schemas, and the shell finds
    its data.
  - Libraries resolve through the prefix binaries' RPATH. Nothing gets
    `LD_LIBRARY_PATH`, so applications started in the session load the
    system's libraries.
  - The prefix links in the system typelibs and GIO modules (dconf, gvfs)
    that it does not build. GLib keeps backward ABI, so the system's GTK,
    gcr, evolution-data-server and gnome-desktop libraries run unchanged on
    the prefix's newer GLib inside the compositor process.
  - The private PipeWire daemon stays the system's. Mutter 51 links the
    prefix's libpipewire 1.6 and speaks the native protocol to it.
- **Qualification.**
  - The record's stack identity gains `compositor_prefix` (absent for
    `/usr`, so existing records keep their identity).
  - Changing the prefix, or the version inside it, makes the record stale
    (`compositor_prefix` / `compositor_version`), and the Pier re-qualifies
    at its next start.
  - Every capability the newer compositor brings is still advertised only
    once that host's qualification proves it: Grading/HDR through the
    fidelity probes, Speed through the measured rates.
- **Installer.** It probes the configured prefix instead of `/usr` when the
  existing configuration names one. It never builds or installs a prefix;
  that is an operator action (admin guide, "Upgrading the GNOME session on
  EL10").
- **SELinux.** `semanage fcontext -a -e /usr <prefix>` gives the prefix the
  labels `/usr` has (`bin_t`, `lib_t`, `usr_t`). No policy module changes.
  `-d -e` reverses it.

### The lab prefix

`packaging/linux/tools/gnome-prefix/` builds the prefix in a
`quay.io/rockylinux/rockylinux:10` container, the same base as the host. No
development package touches the host.

Pinned versions:

- glib 2.90.1, gobject-introspection 1.86.0;
- wayland 1.26.0, wayland-protocols 1.49;
- libinput 1.31.3, libei main (1.7 development);
- PipeWire 1.6.9 (client libraries and modules);
- gsettings-desktop-schemas 51.0, glycin 2.2.3;
- SpiderMonkey from Firefox 140.17.0esr, gjs 1.90.1;
- Mutter 51.1, GNOME Shell 51.1.

Two Mutter merge requests are applied **as lab patches only**. Each one is
recorded with its source and digest in the prefix's `PROVENANCE.md`:

| Patch | Status upstream | Why |
| --- | --- | --- |
| mutter!5358 "stream-source: Overhaul 'too early' computing" | merged on main 2026-10-14, after 51.1 | 60 Hz screen-cast pacing |
| mutter!5224 "seat/native: Enable libinput in headless", plus an Arcen seat selector (`ARCEN_LAB_HEADLESS_INPUT_SEAT`) | open | libinput in a headless compositor, for a real pen on a private udev seat ([Pen and Hard USB](#pen-and-hard-usb-in-a-wayland-session)) |

Shipping depends on upstream. Until those changes are released, a prefix
built with them is a lab prefix.

### Lab evidence (2026-10-19, Rocky Linux 10.2 / NVIDIA GRID V100D 580.105.08)

**Build.**

- The build image comes from `quay.io/rockylinux/rockylinux:10` with the
  `crb`, `devel` and source repositories, plus a meson 1.9 virtualenv.
  EL10's meson 1.4 is older than Mutter 51 needs.
- glycin 2.2 needs rustc ≥ 1.93, so it builds with a pinned rustup 1.96.1
  inside the build tree. SpiderMonkey keeps EL10's rustc 1.92.
- SpiderMonkey 140 needs an out-of-tree object directory and
  `#define XP_UNIX` in the installed `js-config.h` (mozilla bug 1973994,
  which Fedora patches too).
- Wall time with 16 jobs on 24 vCPU, after the image: about 25 min, most of
  it glycin's crates.
- Result: 733 MB in `/opt/arcen-gnome/51`, every library resolved
  (`finish-prefix.sh`), labelled `bin_t`/`lib_t`/`usr_t` through
  `semanage fcontext -e /usr`. No `-devel` package and no dnf transaction
  on the host.

**Qualification.** Development records were made from the same Pier
build (`wayland qualify --record`, three operator sizes).

| | System GNOME Shell 49.4 | Prefix GNOME Shell 51.1 |
| --- | --- | --- |
| Session ready | 2.4–2.9 s | 2.4–2.7 s |
| GPU import | ok, explicit sync | ok, explicit sync |
| Speed 2560x1440@60 and 1920x1080@60, h265 and h264 | sustained 60 | sustained 60 |
| Xwayland applications | ok | ok |
| Session audio monitor | ok (private graph) | ok (private graph) |
| EIS stylus | `stylus_not_offered` | `client_lacks_stylus`: the prefix's libei main negotiates `ei_stylus` v1, but Mutter's seat offers no stylus device |
| Grading / HDR | refused: Mutter 49.4 has no HDR screen sharing | route `force-hdr`, `RecordMonitor` xBGR2101010 DMA-BUF, both ramps 1024/1024 codes with max error 0, SDR white 202.9 nits (see below) |
| Two outputs (`--multi-output 2`) | ok: Meta-0 and Meta-1 at 1920x1080@60 side by side, GPU route | ok: same |

Two qualification bugs turned up on GNOME Shell. Both are fixed on this
branch:

- **The ramp was measured under the Activities overview.** GNOME Shell
  opens the overview at login, so the ramp was a scaled thumbnail: all 1024
  codes present, max error 810. The fidelity probes had been proven under
  bare Mutter, which has no overview. The fidelity step now leaves the
  overview first, as the Speed steps do. After that, both ramps measure
  max error 0.
- **Ten-bit Wayland capture was forced to a local cursor.**
  `apply_capture_cursor_constraint` applied the Xorg rule (an RGB10 XShm
  capture cannot draw the host cursor) to Wayland too. A Grading/HDR capenc
  in an embedded-cursor session was then refused by capenc's own
  cursor-authority check. A Wayland stream now keeps the authority its
  stream chose.

**Pen on the distribution's GNOME 49.** `/opt/arcen-gnome/49` holds only:

- `lib64/libmutter-17.so.0`: Rocky's `mutter-49.4-4.el10_2` SRPM rebuilt
  with the same lab edit;
- a `bin/gnome-shell` wrapper that runs `/usr/bin/gnome-shell` with that
  library first on the library path.

It qualified like the system GNOME (Speed 60, GPU import). With
`wayland.lab_seat_pen` the real Deck GUI's pen reached Firefox in the system
GNOME Shell 49.4 as `pointerType: "pen"`: 1009 moves, 33 pressure levels
(0.037–0.638). Glass ran at 57.3 fps (Speed); motion-to-photon was 110.5 ms
p50 over the VPN link.

**Real screen.** The real Deck GUI (cursor-local build) was driven by the
real-screen harness against a real Firefox in the session. Deck-side
numbers were measured over the Norway→London VPN link and are p50 (p95), in
milliseconds.

| Run | Glass fps / page fps | Cursor on glass | Input → host event | Motion-to-photon |
| --- | --- | --- | --- | --- |
| System 49, Auto, mouse | 29.9 / 59.5 | 10.1 (11.0) | 19.2 (24.9) | 118.4 (127.9) |
| Prefix 51, Auto, mouse | 30.0 / 59.9 | 15.3 (16.3) | 23.4 (30.0) | 123.7 (140.2) |
| System 49, Speed, mouse | 59.1 / 59.7 | 11.2 (12.1) | 19.5 (24.7) | 103.6 (120.1) |
| Prefix 51, Speed, mouse | 59.1 / 59.6 | 12.4 (13.6) | 21.2 (27.9) | 120.8 (137.1) |
| Prefix 51 + lab seat pen, Speed, pen | 59.5 / 60.1 | 14.7 (90.1) | 23.4 (31.2) | 123.2 (131.8) |
| System 49 + patched libmutter-17 + lab seat pen, Speed, pen | 57.3 / 60.0 | 10.6 (85.6) | 19.2 (28.9) | 110.5 (152.1) |
| Prefix 51, HDR (served `hdr`, `rgb10-direct`, HEVC 4:4:4 10-bit PQ), mouse, before the input fix | 29.9 / 59.7 | 13.6 (14.8) | n/a | n/a |
| Prefix 51, Grading (served `grading`, `rgb10-pq-to-sdr`), mouse, before the input fix | 29.8 / 59.8 | 13.6 (14.7) | n/a | n/a |
| Prefix 51, Grading, mouse, input follows the wide epoch | 29.9 / 59.6 | 14.3 (15.2) | 22.3 (28.5) | 131.7 (147.4) |
| Prefix 51, HDR, mouse, input follows the wide epoch | 30.0 / 59.5 | 15.2 (16.3) | 23.5 (26.8) | 125.0 (140.9) |

- The cursor stayed local (Deck-drawn) in every run. The Pier chose the
  metadata cursor stream (`cursor_metadata_proven=true`).
- 49 and 51 feel the same over this link. The differences sit inside the
  link's run-to-run spread, so they are not a verdict.
- 51's value today is the pen (with the lab patch) and the HDR route, not
  mouse latency.
- The pen run is the first real pen in a headless Wayland session (see
  [Pen and Hard USB](#pen-and-hard-usb-in-a-wayland-session)).
- **Grading and HDR, end to end.** After the two fixes above, the 51.1
  host qualified Grading and HDR. The real Deck was then served `hdr` and
  `grading` at the contract's 30 fps from a forced-HDR desktop (capenc
  DMA-BUF import about 9 ms, convert about 8 ms, NVENC about 5.5 ms).
- **Absolute input in a wide epoch (fixed).** In the first two Grading and
  HDR sessions the page received clicks at (0, 0) and no pointer motion:
  0 move events, and calibration failed. The cause was in Mutter.
  - It applies an absolute coordinate given for a `RecordVirtual` stream
    only while that stream is consumed (`transform_position` needs the
    stream's source enabled). Otherwise `NotifyPointerMotionAbsolute`
    drops it as "early".
  - A wide epoch consumes its own `RecordMonitor` streams in a separate
    ScreenCast session, so the virtual streams that input addressed were
    idle.
  - While a wide epoch runs, `MutterSessions` now addresses absolute
    pointer and touch input to unstarted `RecordMonitor` streams of the
    same outputs, recorded in the `RemoteDesktop`-linked session. A monitor
    stream maps through the logical monitor's layout whether or not
    anything consumes it.
  - They are reused across epochs (Mutter keeps them until the session
    ends) and released when the epoch ends. Each output keeps its own
    target, so multi-output layouts map per connector.
  - Afterwards: Grading 1562 moves and HDR 1525 moves. Both clicks landed
    where the harness put them (720, 700), with 25 calibration pairs and
    the cursor local throughout. Grading and HDR are now drawing routes on
    this stack.

## Public host-local API

The public API is intentionally confined to `arcen-pier-linux`:

- `display::wayland::WaylandOutputSource` is the feature-gated future
  output-inventory seam. It reports a compositor snapshot; it does not own or
  mutate a display transaction.
- `display::wayland::WaylandOutputCapabilities` and
  `display::wayland::OutputCapabilityReport` contain compositor detection
  evidence. They are not the shared `arcen_outputs::OutputCapabilities`
  contract.
- `display::wayland::{WaylandOutput, WaylandOutputSnapshot}` combine coherent
  `wl_output` mode/scale/transform state with `xdg-output` logical regions.
- `display::wayland::detect_output_capability` evaluates compile-time and
  authoritative runtime facts. Unknown protocol state is unavailable.
- `input::eis::InputProvider` is the feature-gated future portal/libei seam
  for output regions; the injection seam is the session agent's
  `session::wayland::input::CompositorInput` (see [Input](#input)).
- `input::eis::{EisRegion, EisRegionMap}` reconcile compositor-advertised EIS
  regions with current Arcen logical regions and map region-local coordinates.
- `input::eis::detect_input_capability` requires established output regions,
  RemoteDesktop portal availability, an EIS connection, and an absolute
  pointer capability.

These interfaces consume the existing pure `arcen-media` region value objects:
`RegionId`, `RegionGeneration`, `LogicalRect`, `PhysicalSize`, `Scale120`, and
`OutputTransform`. Native Wayland/EIS handles and provider traits do not enter
shared crates or the wire protocol.

## Geometry rules

- `xdg-output` position and size are the authoritative logical desktop region.
  They are converted from whole compositor logical pixels into Arcen's
  1/120-logical-pixel fixed-point domain.
- `wl_output` mode is retained as the explicit pre-transform physical extent.
- All eight `wl_output.transform` values map directly to the shared transform
  vocabulary.
- Integer `wl_output.scale` converts to `Scale120` by multiplying by 120.
- A fractional preference may override it only when a future provider can
  authoritatively associate the surface-scoped preference with that output.
  Merely observing `fractional-scale-v1` does not establish a global
  per-output scale.
- EIS regions use unsigned desktop-wide logical offsets. The pure mapper
  translates a Wayland layout's minimum signed origin to zero while preserving
  relative placement.
- EIS mapping IDs are preferred but not trusted as unique. During resize, an
  exact geometry match may disambiguate duplicate IDs; disagreement or
  ambiguity fails closed. Exact geometry is the fallback when IDs are absent.
- EIS physical scale and Wayland presentation scale are retained separately as
  `Scale120` metadata; neither is derived from or forced equal to the other.
  EIS physical scale does not alter absolute logical region coordinates.

## Capability gates

| Gate | Current result |
| --- | --- |
| Cargo feature absent | `FeatureDisabled` |
| Non-Linux target | `UnsupportedTarget` |
| Session/socket/core-protocol fact unknown | Typed unavailable reason |
| Missing `wl_output` or `xdg-output` | Typed unavailable reason |
| Native Wayland adapter | Not implemented |
| RemoteDesktop portal/EIS grant | Must be authoritatively supplied; no heuristic |
| Native libei adapter | Stylus probe only (`session::wayland::eis`, `reis`); no libei injection |
| Mutter virtual output | This detection seam still reports detected-but-unimplemented; the headless session's virtual outputs are the separate `MutterVirtualOutputProvider` above |

## HDR provider requirements

Wayland is also the boundary for future Linux desktop HDR. Selecting the Deck's
HDR preset must not be enough by itself. A native provider may retain PQ/HLG
only after it proves all of the following for the captured output:

- the compositor reports an active color-managed HDR output/image description;
- primaries and transfer characteristics are authoritative, not inferred from
  ten-bit storage;
- capture negotiates a format that preserves the compositor's HDR values
  (FP16 or a documented ten-bit format) rather than an 8-bit PipeWire fallback;
- SDR reference white and mastering/content-light metadata are available where
  the transfer requires them; and
- capture, input/EIS, resize, teardown, and reconnect remain bound to the same
  compositor output generation.

This future provider is a third Linux capture pipeline, not a replacement for
the two proven Xorg paths. Auto/Speed must continue to use NvFBC where its
device-to-device advantage is available, and Xorg Grading must continue to use
depth-30 XShm. Provider selection happens from explicit session/capability
truth; adding Wayland HDR must not route all sessions through PipeWire or add a
host copy to the eight-bit fast path.

On Wayland these facts are now implemented for compositors that have HDR
screen sharing (Mutter ≥ 50): see
[Wayland Grading and HDR](#wayland-grading-and-hdr-wp-fidelity). Dedicated
Xorg still resolves HDR requests to Grading Reference (HEVC 4:4:4 10-bit full-range BT.709). The Deck
shows matrix/primaries/transfer degradation and remains in SDR presentation
mode. Depth 30 proves precision, not HDR. The one exception is a desktop the
operator declares Rec.2100 PQ (`video.desktop_encoding`), where an
application such as Flame writes PQ itself; see `color-fidelity.md`.

`WaylandRuntimeFacts::from_process_environment` proves only the Wayland session
marker and Unix socket. It deliberately leaves protocol state unknown because
file presence or environment variables cannot prove registry, portal, or EIS
capability.

## Headless HDR spike result (2026-09-27)

A headless GNOME 50 session delivered a bit-exact PQ / BT.2020 ten-bit
desktop through PipeWire on the Linux lab (NVIDIA GRID V100D-16Q, driver
570.172.08, Rocky Linux 9.5 host, no display connectors). This is evidence
for the future provider, not a shipped pipeline.

Pipeline measured:

- **Session:** Mutter 50.5 (Fedora 44 container, podman with NVIDIA CDI)
  in headless mode on the NVIDIA render node through GBM, with one virtual
  monitor in `bt2100` colour mode (`gdctl set ... --color-mode bt2100`).
  The host needed `nvidia-drm modeset=1`; the Xorg pipelines were
  re-measured unchanged with it.
- **Content:** a Wayland client presenting an `XRGB2101010` shared-memory
  buffer tagged PQ / BT.2020 through `wp_color_management_v1`, carrying a
  0..1023 ramp.
- **Capture:** `org.gnome.Mutter.ScreenCast` `RecordMonitor`; the PipeWire
  stream advertises `xRGB_210LE`, `xBGR_210LE` and `RGBA_F16`, each tagged
  `colorPrimaries=BT2020` and `transferFunction=SMPTE2084`, as DMA-BUF and
  shared memory; a consumer asking for ten-bit PQ negotiated `xRGB_210LE`.
- **Result:** the captured frame held all 1024 codes, exactly, grey, and up
  to PQ 1023 (10,000 nits), so highlights above the 203-nit SDR white
  survive.

Stock Mutter 50.5 does not get there. Four changes were needed, each small,
and two of them are defects that label eight-bit pixels as ten-bit HDR:

1. Virtual outputs declare no colour spaces or HDR transfer functions, so a
   virtual monitor never offers `bt2100`. Declaring BT.2020 and PQ on them
   enables it.
2. A virtual monitor composites into a default eight-bit texture, where a KMS
   output prefers ten-bit scanout; it must composite into ten-bit.
3. The shared-memory screencast path paints every frame as eight-bit ARGB
   whatever format was negotiated, so an `xRGB_210LE` PQ stream carried
   256-level pixels. It must paint in the negotiated format.
4. The paint-to-buffer helper renders through a default eight-bit texture
   before reading back; it must render at the requested precision.

Arcen must not depend on a privately patched compositor: these belong
upstream, and the product capture should use DMA-BUF (the zero-copy route
into CUDA/NVENC), which does not pass through defects 3 and 4. KWin was not
chosen: its virtual backend sets no HDR capability and its HDR output
screencast change (plasma/kwin!9843) is closed unmerged. gamescope's
PipeWire output is eight-bit only.

### End to end to a Deck (2026-09-27)

A lab-only lever, `ARCEN_EXPERIMENTAL_RGB10_PIPE=<fifo>` (default off, a
systemd drop-in on the lab only), serves an HDR request from a FIFO that a
PipeWire consumer inside the container fills with tagged PQ / BT.2020 frames
(`capenc rgb10-pipe=`, `WideSource::ColorManagedPq`, READY capture backend
`pipewire`). A real Deck showed HDR: `rext 4:4:4 10-bit bt2020/pq/bt2020ncl`,
950-970 distinct luma codes, and 27.8-28 fps sustained for 120 s with no
dropped frames. Two defects found on the way are fixed: the pipe loop never
started the keyframe control thread (a late Deck waited forever), and the
consumer must open its FIFO before connecting the stream, not inside a
process callback. Input still reached Xorg, so this measures the provider's
video path only.

A host process can consume the container's PipeWire directly (host
libpipewire 1.0.1 against the container's 1.6.9 server over a shared runtime
directory), which is the route for in-process capture without a helper.

## Shared capture, admission and provider-selection contracts

[ADR 0014](../adr/0014-quic-lanes-and-capture-contracts.md) adds the portable
contracts a native provider will implement. They are pure shared code; no
Linux adapter calls them yet.

- `arcen_media::video::CaptureSurface` describes each delivered buffer as
  data — output and generation, geometry, pixel format, per-plane layout,
  opaque DRM-style modifier, source colour, timestamps, damage, cursor, GPU
  identity and copy path — and `validate` refuses truncated layouts, short
  strides, unproven or implicit modifiers, unproven or mismatched GPUs, stale
  generations and mid-epoch colour changes. Missing damage is full damage.
- `arcen_media::video::CaptureMailbox` (`capture_lease`) is the bounded,
  latest-wins lease queue between a capture callback and its import worker:
  superseded buffers go straight back to the producer with their damage
  carried forward (`DamageEvidence::union`), a lease returns its buffer only
  when dropped, and `PendingCaptureDamage` applies carried damage to Keel
  until submission. Adapter buffer handles stay opaque tokens.
- `CopyPath` reports per-path cost (`zero-copy-import`, `gpu-convert`,
  `host-copy`, `cross-gpu`). `CaptureBackend::PipeWire` keeps
  `zero_copy() == false`; a DMA-BUF import proves zero copy per path, not per
  backend.
- `arcen_media::video::admit_capture_source` decides Auto, Speed, Grading and
  HDR separately from `CaptureSourceEvidence` and the encoder's limits. Auto
  and Speed take only eight-bit SDR capture; Grading needs authoritative
  native-wide samples and colour; HDR additionally needs an authoritatively
  active HDR output and falls back to Grading only when Grading is genuinely
  admissible. The defects listed in the spike above — eight-bit pixels in a
  ten-bit format — are exactly what `SamplePrecision::UpconvertedEightBit`
  refuses.
- `arcen_session::desktop_provider` reads `capture.provider` (`auto`
  default, `xorg` and `wayland` overrides; unknown tokens fail
  configuration loading). A Linux Pier built without `wayland-provider`
  refuses `wayland` at configuration load; Windows and macOS Piers refuse
  the key entirely. `select_desktop_provider` chooses from
  `HostDesktopFacts` (ADR 0014 Amendment 4): Xorg when an Xorg server is
  installed, Wayland on a host without one when compositor, session
  service, PipeWire, a DRM render node, a capture route and input
  injection are each proven (the last two by the qualification record;
  Amendment 3 replaced GPU import with the capture route for Auto/Speed),
  otherwise a refusal naming each fact, with no fallback and no migration
  of existing installations. See [Provider selection](#provider-selection).

## Capture leases and GPU import (`wlp-import`)

`hosts/capenc/src/wayland/` (Cargo feature `wayland`, default off) consumes
the `PipeWire` node the session adapter creates. The Xorg NvFBC and XShm paths
and their eight-bit fast path are unchanged; nothing selects this code yet.

**Hand-off.** capenc never talks D-Bus. It reads the shared hand-off
contract described under [Capture hand-off to capenc](#capture-hand-off-to-capenc)
with the shared parser (`arcen_media::video::capture_handoff`);
`wayland/handoff.rs` only adapts it to the process environment. A stream
whose negotiated size is not `ARCEN_CAPTURE_EXPECTED_SIZE` is refused: the
node no longer records the output that was handed over.

**Quiet streams are not dead.** GNOME 40 ScreenCast delivers a buffer only
when the output changes; an idle desktop produces none for as long as it
stays idle. A consumer must not time out, renegotiate or report the
producer lost because no buffer arrived, and the first-frame readiness
proof needs some damage, such as a client that draws (cursor-only buffers
carry no pixels).

**Bounded leases.** One thread owns the `PipeWire` loop and stream. The
process callback only dequeues, classifies the buffer as plain data
(`layout.rs`), validates it with `CaptureSurface::validate`, and pushes a
lease into the shared bounded latest-wins
`arcen_media::video::CaptureMailbox`. It never waits on
conversion, upload, encode, GPU or network work and never logs; it counts.
A superseded frame is requeued at once and its damage is carried into its
successor with `DamageEvidence::union` (missing damage is full damage); the
worker unions it into Keel through `PendingCaptureDamage`
(`PendingExternalDamage`) until a picture is submitted. A size-0
buffer updates cursor metadata only. Corrupted or refused buffers are
requeued with their pixels unread and their (unknown) damage is carried
forward (cursor metadata is still kept from every buffer whose header is
not marked corrupted, because Mutter sends a sprite once), as is the
damage of queued frames revoked by renegotiation; a format change carries
full damage. Cursor metadata of one process call is coalesced (latest
position, latest bitmap) and applied under a lock readers hold only to
clone, so a shape sent once is never dropped. A lease
returns its buffer to `PipeWire` only when dropped, after the synchronous
copy that read it; only the loop thread requeues. Buffer removal or a format
change takes a write lock that waits for an in-progress read and revokes
every outstanding lease, so a revoked buffer is never read or requeued.

**Import routes, each with a truthful `CopyPath`:**

| Route | Memory | `CopyPath` | Status |
| --- | --- | --- | --- |
| host-copy upload: one 2D host-to-device copy into the retained CUDA frame (padding dropped); NVENC converts BGRA as on the NvFBC path | `MemFd`/`MemPtr` BGRx/BGRA | `host-copy` | **proven** (GNOME 40, GNOME 49) |
| GPU convert (`gpu_convert.rs`, `wlp-gpu-speed`): EGL device on the encoder's render node, `eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT)` imported as `XB24`/`AB24`, `glBlitFramebuffer` into a `GL_RGBA8` texture registered with `cuGraphicsGLRegisterImage`, GL fence wait, then map and `cuMemcpy2D` into the retained frame | DMA-BUF BGRx/BGRA with an explicit, non-external-only modifier | `gpu-convert` | **proven** on GNOME 49 / NVIDIA 580 with `nvidia-drm modeset=1` (see [GPU import and the paced Speed output](#gpu-import-and-the-paced-speed-output-wlp-gpu-speed)) |

Neither route is zero-copy, and no route reads pixels back to the CPU except
the host copy, whose producer already rendered into host memory. DMA-BUF is
offered to a producer only when capenc's importer opened and passed its
GL-to-CUDA self-test on the encoder GPU, the compositor is that same GPU
(unknown or different refuses the route: no cross-GPU path), EGL lists a
modifier for both the producer's format and its swizzled import, and the
Pier passed `gpu-import=verified` because the host's qualification record
proves the route. Otherwise the epoch offers shared memory and logs the
reason (`Wayland capture offer … not_offered=dmabuf-gl-blit reason=…`).

**Synchronisation.** With DMA-BUF offered, capenc asks for `PipeWire` explicit
sync (`explicit_sync.rs`): a `Buffers` param with two extra blocks and a
mandatory `SPA_META_SyncTimeline`, then the `SyncTimeline` meta. Mutter 49
grants it when its DRM device has timeline syncobjs; the two trailing
`SyncObj` blocks never become planes. The import worker waits for the
acquire point (`DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`, 100 ms bound) before the
blit reads the buffer and waits for the blit's GL fence before it drops the
lease; the `PipeWire` loop thread signals the release point
(`DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL`) immediately before it queues any such
buffer back — imported, superseded, refused or cursor-only alike — and
clears the producer's unscheduled-release flag only after that signal
succeeded. A failed signal leaves the flag set (a producer that honours it
releases the buffer itself) and is counted; three in a row withdraw DMA-BUF
for the epoch, so a pool that stops being released never stalls the stream
silently. A
DMA-BUF without sync points is synchronised implicitly: the worker waits
for the buffer's exported read fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`) and
refuses the import when the kernel cannot export one.

**Failure.** Any import failure releases what it created, encodes nothing
from that frame and does not re-encode the retained frame until a frame
imports again; capenc then withdraws DMA-BUF from the offer
(`PipeWireSource::fall_back_to_host_memory`, `gpu import fallback: …`) and
the producer renegotiates to shared memory for the rest of the epoch.

Telemetry: the worker emits `CAPTURE_PATH_SELECTED` with the ADR 0014
fields (copy path, memory, modifier class, GPU match, source colour,
precision, cursor) through a bounded `arcen-observability` sink.

### Lab evidence (2026-10-17, GNOME 40 baseline host)

Rocky Linux 9.7, `mutter-40.9` headless with one 1920x1080 virtual monitor,
`pipewire-1.0.1`, GRID V100D-16Q, driver 580.65.06, as a throwaway user;
`capenc wayland-capture-probe` (release build) with
`scripts/lab/wayland-capture-probe.sh`:

- Negotiated `BGRx` 1920x1080, stride 7680, `MemFd` shared memory, framerate
  0/1 with the requested max framerate (60/1, or 30/1 when asked for 30).
  Every frame validated as `host-shared` / `host-copy`; damage `unknown`
  (full: 8160 Keel blocks); cursor as metadata, bitmap serials advancing with
  pointer motion.
- CUDA read-back of the host-copy upload equalled the CPU's tightly packed
  rows on every checked frame (5/5, 3/3, 2/2, 1/1 across runs).
- 120 frames with an animated client: 37.5 fps delivered (Mutter-limited, as
  in the baseline), upload 0.83 ms average / 1.17 ms max, NVENC H.264 encode
  2.82 ms average, 120 valid access units. HEVC: SPS `main 4:2:0 8-bit
  bt709/bt709/bt709 limited`, encode 3.8 ms average.
- Callback cost 12–14 µs average, 42 µs max; at most two leases outstanding,
  none left at teardown; zero corrupted, refused or validation-refused
  buffers.
- With the DMA-BUF offer (render node matched, GPU `same`), the
  modifier-bearing format was accepted and Mutter still delivered shared
  memory (it disables DMA-BUF when surfaceless); EGL on `renderD128` reported
  twelve block-linear modifiers plus external-only `LINEAR` for `XR24`.
- Plain Mutter delivered no size-0 cursor-only buffers in these runs (pointer
  motion rode on full frames); cursor-only handling is covered by unit tests
  only.

Not established on GNOME 40: DMA-BUF import of a real compositor buffer
(Mutter 40 sends none), rates above Mutter 40's ~37 fps, 4K, and Deck
end-to-end. GNOME 49 established the DMA-BUF route; see
[GPU import and the paced Speed output](#gpu-import-and-the-paced-speed-output-wlp-gpu-speed).

## GPU import and the paced Speed output (`wlp-gpu-speed`)

Code: `hosts/capenc/src/wayland/gpu_convert.rs` (the route),
`explicit_sync.rs` (DRM timeline syncobjs), `pipewire.rs` (sync blocks,
release signalling, the shared-memory fallback), `pipelines/runner.rs` (per
frame route dispatch) and `probe.rs` (`verify-gpu-import`); on the Pier,
`Config::wayland_gpu_import_verified`, `Config::wayland_session_refresh_hz`
and `wayland qualify`. Registered as `linux-wayland-gpu-import`
(validation `linux-wayland-gpu-import-lab`). ADR 0014 Amendment 5.

**Why a GL blit.** On NVIDIA 580 with `nvidia-drm modeset=1`, Mutter 49's
DMA-BUFs carry block-linear modifiers (`0x0300000000cdb014`), and
`cuGraphicsEGLRegisterImage` refuses their EGL images; so does
`cuGraphicsGLRegisterImage` on a texture backed by the image. A GL blit from
the image into an ordinary `GL_RGBA8` texture that CUDA registered once per
epoch reads the buffer correctly. The route imports the producer's `XR24`
(`AR24`) bytes as `XB24` (`AB24`) — the same 32-bit layout, only the channel
names exchanged — so the blit stores B, G, R, X and the CUDA copy of the
texture yields exactly the BGRA input NVENC already takes from the host-copy
route. Only modifiers EGL can import as a `GL_TEXTURE_2D` in both formats are
offered (twelve block-linear modifiers on this stack; `LINEAR` is
external-only and never offered). At start the importer clears its texture
and reads it back through CUDA (the GL-to-CUDA self-test); a failure leaves
the epoch on shared memory with the reason.

**Who may use it.** capenc offers DMA-BUF only with `gpu-import=verified`
from the Pier, which passes it only when the host record proves GPU import:
`wayland qualify` runs `wayland-capture-probe verify-gpu-import` in the
qualification session — the first DMA-BUF frame is imported and read back,
DMA-BUF is withdrawn, and the shared-memory frame of the same still desktop
is the reference; at most 0.1 % of pixels may differ, and the reference must
itself tell a red/blue swap apart — then runs Auto and Speed on the
verified route and records `gpu_import_verified_frames`.

**The paced Speed output.** Mutter's ScreenCast drops a frame that comes
within one interval of the consumer's maximum frame rate after the previous
one; a *monitor* stream (`RecordMonitor`: the `--virtual-monitor` harness
below, GNOME 40's startup monitors) cannot ask for more than the monitor's
refresh, so a 60 Hz monitor with a 60 fps maximum delivers 36–41 fps of a
60 fps client. A Mutter 49 `RecordVirtual` output (what the Pier uses there)
is not limited that way and holds 60 fps at 60 Hz
([Speed without operator sizes](#speed-without-operator-sizes-wp-speed));
the paced output below remains for stacks where only it holds Speed. Speed therefore asks the producer for
twice its ceiling (`paced_source_offer_fps`, 120), and a session whose
authenticated request resolved to Speed creates its virtual output at
`paced_output_refresh_hz` (120 Hz for a 60 Hz display) — only when the host
record qualifies Speed at that size and refresh; every other session keeps
the Deck display's refresh. The output is created once per session: a
later Speed request on a session created at 60 Hz is visibly served as Auto
unless 60 Hz itself qualifies. Qualification creates its session the Speed
way for each `--output`, and records the rate per output refresh as the
frames per second both delivered and encoded (an encoder that cannot keep
up caps it).

### Lab evidence (2026-10-17/18, Rocky 10.2 / GNOME 49.4 / NVIDIA GRID V100D 580.105.08, `modeset=1`)

Raw `mutter --headless` as a throwaway local user
(`scripts/lab/wayland-capture-probe.sh`), debug capenc, animated GTK client
(16 ms timer) unless stated; CPU is percent of one core, GPU and encoder
utilisation from `nvidia-smi`.

| Check | Result |
| --- | --- |
| GPU import vs host copy, still pattern (red/blue ramps, green ramp, checkerboard), 1920x1080 | `result=ok`, 2 073 600 pixels, **0 mismatched**, discriminating, `sync=explicit` |
| Same at 3840x2160 | `result=ok`, 8 294 400 pixels, **0 mismatched**, `sync=explicit` |
| Explicit sync | every DMA-BUF frame carried acquire/release points (601/601 at 1080p@120, 726/726 at 4K@120); every release point signalled, 0 failures |

| Run | Output | Route | Source fps | Encoded fps | Import (sync / blit / CUDA copy) ms | NVENC ms | Mutter CPU | capenc CPU | GPU / ENC % |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Speed H.264 1080p | 120 Hz | `gpu-convert` | 60–62 | **60–61** | 2.3–2.6 (0.01 / 2.3–2.5 / 0.04) | 2.5 | 26 % | 22 % | 9 / 4 |
| Speed H.264 1080p | 120 Hz | `host-copy` | 60 | **60–61** | 0.94–0.99 upload | 2.8 | 46–48 % | 15 % | 16 / 5 |
| Speed HEVC 4K | 120 Hz | `gpu-convert` | 60–61 | **51–52** (NVENC-bound) | 2.8–2.9 | 14.1 | 25 % | 30 % | 11 / 22 |
| Auto H.264 1080p | 60 Hz | `gpu-convert` | 24–25 | 30–31 | 2.7–2.8 | 2.4 | 24–26 % | 9–12 % | 8 / 2 |
| Auto H.264 1080p | 60 Hz | `host-copy` | 20 | 30 | 0.9–1.1 upload | 2.4 | 33–35 % | 5–7 % | 10 / 2 |
| Auto H.264 1080p, moving | 120 Hz | `gpu-convert` | 25 | 30–31 | 2.6–3.0 | 2.9–3.1 | 25–26 % | 11 % | 8 / 2 |
| Auto, still desktop | 60 Hz | `gpu-convert` | 0 | 1 (keepalive) | — | 3.4 | 0 % | 0 % | 0 / 0 |
| Speed, still desktop | 120 Hz | `gpu-convert` | 0 | 1 (keepalive) | — | 3.3 | 0 % | 0 % | 0 / 0 |
| Probe, 1080p, 60 fps offer | 60 Hz | `gpu-convert` | 37.7 | — | 2.5 | — | 26 % | — | 8 / 3 |

Readings: the GPU route halves the compositor's CPU (Mutter no longer reads
the frame back into shared memory) at about 2.5 ms of import per frame,
almost all of it the blit and its fence (it hardly changes from 1080p to
4K, so it is fixed overhead, not bandwidth); a 120 Hz output costs nothing
on a still desktop (no frames, 0 % CPU, one keepalive a second) and nothing
extra for a client that draws on its own timer; 60 fps needs the 120 Hz
output either way (37.7 fps at 60 Hz). 4K Speed is bound by NVENC HEVC at
14 ms a frame in this serial loop (51–52 fps encoded); qualification
measures with H.264, which keeps up at 4K, and counts only frames both
delivered and encoded.

GNOME Shell (`ARCEN_LAB_COMPOSITOR=gnome-shell`, same harness, Speed on the
`gpu-convert` route, a rate asserted only to run the encoder at 60):

| Content | Output | Source fps | Encoded fps | gnome-shell CPU | capenc CPU | GPU / ENC % |
| --- | --- | --- | --- | --- | --- | --- |
| 60 fps GTK client | 120 Hz | **60–62** | 59–61 | 33–38 % | 23–25 % | 11 / 5 |
| pointer every 16 ms (RemoteDesktop, embedded cursor) | 120 Hz | **60–62** | 60–61 | 9–12 % | 18–20 % | 3 / 5 |
| 60 fps GTK client | 60 Hz | 38–40 | 60 (flushes) | 32–34 % | 17–18 % | 10 / 4 |

**Product qualification and a Deck** (release Pier, `arcen-pier wayland
qualify --write --output 1920x1080@60 --output 1800x1130@60 --output
3840x2160@60` as the temporary `arcen-wlq` account, then the Pier serving
the macOS Deck's `media-smoke` from a lab Mac; Pier in an `unconfined_t`
transient unit):

| Step | Result |
| --- | --- |
| Sessions | `gnome-shell` at 1920x1080@120, 1800x1130@120 and 3840x2160@120 (the paced Speed output for each 60 Hz display) |
| `gpu_import` | `result=ok`, 0 of 2 073 600 / 2 034 000 / 8 294 400 pixels differ, `sync=explicit`, first attempt each |
| `capture_route` | Auto admitted on `gpu-convert`, first frame `bgrx8` DMA-BUF, 91 access units in 3 s at each size; record: `route gpu-convert`, `gpu_import_verified_frames 1`, GPU import **proven** |
| `speed_rate` (overview left, 60 Hz pointer motion through the session input socket) | delivered/encoded per second (50–53, 60–61) at 1080p, (51–53, 60–61) at 1800x1130, (49–53, 59–61) at 4K H.264: **sustained 52 / 52 / 50 fps, below 57: Speed not qualified** |
| Deck Auto | `active_pipeline=auto`, HEVC 4:2:0 8-bit limited BT.709 1800x1130, decoded by VideoToolbox; capenc `copy_path=gpu-convert route=dmabuf-gl-blit`, encoded 30–31 fps, import 2.4–2.5 ms, NVENC 4.3 ms |
| Deck Speed | visibly served as Auto (`active_pipeline=auto`); the session's output stays at the display's 60 Hz because no rate qualifies 120 Hz |

So on this host the `gpu-convert` route is proven and serves Auto, and the
capture pipeline serves Speed at 60 fps on a 120 Hz output (both
compositors, both routes, lab), but the product's own measurement does not
admit Speed: pointer motion that reaches the compositor through the
session's injector at 60 Hz is recorded at 50–54 fps, against 60–62 fps for
the same motion sent straight to RemoteDesktop every 16 ms and for a 60 fps
client. The shortfall is in the input path's delivery cadence, not in
capture or import; Speed stays visibly Auto until a qualification measures
57 fps.

## Speed without operator sizes (`wp-speed`)

Code: `arcen_session::wayland_qualification` (`resolve_speed`,
`qualification_outputs`, `RequestedSpeedOutputs`, schema 3),
`arcen_media::video::{CaptureRateEvidence, SpeedRuntimeGuard,
admit_speed_after_runtime_shortfall}`, the Pier's
`Config::wayland_speed_capture_rates` / `wayland_session_refresh_hz`,
`session::wayland::note_unqualified_speed_display` and the guard branch of
the attachment loop (`net/server.rs`), and `wayland qualify`. ADR 0014
Amendment 8.

**Upstream.** Mutter's frame limiter is the cause of "60 Hz = 40 fps": in
49.4 `meta_screen_cast_stream_src_maybe_record_frame_with_timestamp` skips a
frame whose timestamp is less than one `maxFramerate` interval after the
last recorded one, and a monitor stream's `maxFramerate` range is capped at
the monitor's refresh, so presentation jitter drops frames. MR !5358
(<https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/5358>, merged to
`main` 2026-10-14, milestone GNOME 51, after the 51.1 tag) rounds to the
nearest interval boundary and fixes it for every stream; it is not on the
`gnome-49`/`gnome-50` branches, and Rocky/RHEL 10.2 ships
`mutter-49.4-4.el10_2` without it (package changelog checked 2026-10-18).
MR !5278 (variable-rate `maxFramerate` for monitor streams, backported to
49.8 and 50.5) and !4881 (49.4) touch the same negotiation. A 49.x
`RecordVirtual` stream reports no refresh (`get_specs` returns false), so its
consumer's own `maxFramerate` applies; on 50.x/51.x a virtual stream with
explicit `modes` reports its mode's refresh, which Arcen does not use. Arcen
does not patch compositors; every stack is measured.

**E1 (2026-10-18, Rocky 10.2 / Mutter and GNOME Shell 49.4 / GRID V100D
580.105.08, `scripts/lab/wayland-capture-probe.sh` in virtual mode with
`ARCEN_LAB_PRE_PROBE`).** A first consumer created the `RecordVirtual`
monitor at 60 Hz (`max-fps=60`); `gdctl` made it the only monitor; a second
consumer of the same size recorded the animated 60 fps client with DMA-BUF.
The monitor stayed at 60 Hz (`DisplayConfig` current mode `@60.000`) in
every run.

| Compositor | Output | Consumer `maxFramerate` | Delivered (capenc probe) |
| --- | --- | --- | --- |
| mutter | 1920x1080@60 | 60 | 59.6 fps |
| mutter | 1920x1080@60 | 120 | 59.7 fps |
| gnome-shell | 1920x1080@60 | 60 | 59.5 fps |
| gnome-shell | 1920x1080@60 | 120 | 59.6 fps |
| gnome-shell | 1800x1130@60 | 120 | 59.6 fps |
| gnome-shell | 3840x2160@60 | 120 | 59.3 fps |
| mutter, `RecordMonitor` of a `--virtual-monitor` (control) | 1920x1080@60 | 60 | 39.0 fps |

**Ladder qualification, no operator sizes** (release Pier, development
record, `wayland qualify --write --config <dev> --allow-running-service`,
temporary account `arcen-wlq-<hex>`, GNOME Shell, `gpu-convert` route
verified at every size, 60 Hz pointer motion through the session socket):

| Display | Output | Codec | (delivered, encoded) fps per second | Sustained |
| --- | --- | --- | --- | --- |
| 1920x1080@60 | 60 Hz | HEVC | (59,61) (60,60) (60,61) (60,60) (60,61) (60,61) (60,61) | 60 |
| 1920x1080@60 | 60 Hz | H.264 | (57,60) (61,61) (60,61) (60,60) (60,60) (60,61) (60,61) | 60 |
| 2560x1600@60 | 60 Hz | HEVC | (59,61) (60,60) (60,61) (60,60) (60,61) (60,60) (60,61) | 60 |
| 2560x1600@60 | 60 Hz | H.264 | (59,61) (58,59) (60,61) (60,60) (60,61) (60,60) (60,61) | 60 |
| 3840x2160@60 | 60 Hz | HEVC | (59,61) (60,61) (60,60) (60,61) (60,61) (60,61) (60,60) | 60 |
| 3840x2160@60 | 60 Hz | H.264 | (57,60) (60,61) (60,60) (60,60) (60,61) (60,61) (60,60) | 60 |

Every display held Speed at its own 60 Hz, so no 120 Hz output was needed
or measured, and every Deck display no wider than 3840 and no taller than
2160 is covered (measured or inferred) for both codecs on this host. HEVC at
4K kept up here (60 encoded of 60 delivered, release build), so it is
admitted as measured; where an encoder does not keep up the measurement
records it and the runtime guard catches a shortfall at serving time.

**Serving a Deck (release dev Pier on another UDP port, 0.17.0 macOS Deck
`media-smoke`, the animated 60 fps client running in the user's session).**
Rates are the Pier's own capenc counts; Deck-side packet rates are over the
Norway→London VPN link and are not evidence of the host.

| Run | Result |
| --- | --- |
| Speed at the Deck's 1800x1130@60 display, no operator sizes (the display had been noted in the requested list by an earlier unqualified request and measured by the next automatic re-qualification) | `active_pipeline=speed`, HEVC; session output 60 Hz; capenc delivered/encoded 60/60–61 fps per second; runtime guard "held its floor" |
| Windowed Deck resized to 1600x1000 (no measurement of that size) | Speed kept, `Wayland Speed rate is inferred from a larger qualified output … evidence=inferred:1800x1168` (HEVC 60, H.264 59 measured there); capenc argv `capture-rate-evidence=inferred:1800x1168`; guard held |
| An Auto attachment, then a Speed attachment of the same user | Auto served on a 60 Hz output; the Speed attachment was served Speed on a 60 Hz output (no 120 Hz output); the smoke opened a new desktop rather than resuming, so a same-desktop switch inside one desktop was not exercised by the CLI |
| Speed while three other 4K HEVC Speed encodes loaded the shared NVENC | capenc delivered 60, encoded 44–45 fps; the guard tripped (`speed_runtime_below_floor`, encoded 44, delivered 60), the capture restarted as Auto at 30 fps inside the attachment with a keyframe, and the Deck printed `served-pipeline-update served=auto backend=hardware reason=speed_runtime_below_floor` and finished with `active_pipeline=auto` |

## Wayland Auto and Speed (`wlp-auto-speed`)

Code: `hosts/capenc/src/wayland/pipelines/` (`auto.rs`, `speed.rs`, the
once-per-epoch dispatch in `mod.rs` and the shared SDR8 mechanism in
`runner.rs`); registered as the separate paths `linux-wayland-auto` and
`linux-wayland-speed` (build profile `linux-capenc-wayland`). capenc runs
them when its environment carries the capture hand-off
(`ARCEN_CAPTURE_PROVIDER`); Xorg sessions never set it, and a capenc built
without `wayland` refuses such a session with `UNAVAILABLE … not_built`.

**Selection, once per epoch.** capenc reads the Pier's usual argv (codec,
fps, priority, intent, `variant=`) and resolves it with the shared
`pipeline_for_request`: the SDR8 format at up to 30 fps with detail
priority is Auto, above 30 fps or with motion priority is Speed. Grading
and HDR are their own compositions (`pipelines/{grading,hdr}.rs`, see
[Wayland Grading and HDR](#wayland-grading-and-hdr-wp-fidelity)); exact
formats have no Wayland composition and are refused with
`UNAVAILABLE … unsupported_configuration`, never served through this path.
No preset flag reaches the `PipeWire` callback, the import or the encode
loop.

**Admission.** Auto asks the compositor for the eight-bit BGRx/BGRA family
at Auto's ceiling, Speed at twice Speed's
(`arcen_media::video::paced_source_offer_fps`; the compositor caps it at the
output's refresh, see [GPU import and the paced Speed output](#gpu-import-and-the-paced-speed-output-wlp-gpu-speed)).
After negotiation capenc takes the first real frame, imports it into a
retained CUDA frame, builds
`CaptureSourceEvidence` (`PipeWire`, the negotiated format and colour, the
route's `CopyPath`, GPU match, cursor truth) and calls the composition's
own shared admission, `admit_auto` or `admit_speed`:

| Request | Evidence on GNOME 40 | Served |
| --- | --- | --- |
| Auto | `host-copy`, colour not reported | Auto, 30 fps, BT.709 limited 8-bit, `colour=assumed` |
| Speed | `host-copy`, no qualified rate | **Auto** (`downgrade`, `copy_path_not_rate_qualified`, `fps_clamped`) |
| Speed, `capture-rate=35` | `host-copy`, qualified 35 fps | Speed at 35 fps (`capture_rate_below_contract`, `fps_clamped`) |
| Speed, qualified ≥ 60 or zero-copy | | Speed, 60 fps |

The epoch then runs the *served* contract (`AdmittedEpoch`): a Speed request
served as Auto runs Auto's cadence, detail priority and two-frame VBV. A
qualified rate is evidence measured on the host, passed as
`capture-rate=<fps>` (the Pier passes the host record's rate for the
session's exact output size and refresh); nothing in the pipeline assumes a
compositor version or rate, so on GNOME 40 Speed is always visibly served as
Auto. capenc opens NVENC for the served contract and only then states the
decision:

```text
[capenc] ADMISSION version=1 requested=speed requested_fps=60
    decision=downgrade reason=copy_path_not_rate_qualified served=auto
    codec=h265 fps=30
    transform=sdr8-rgb colour=assumed cursor_mode=host capture=pipewire
    copy_path=host-copy cursor=composited measured_fps=unmeasured node=36
    format=bgrx8 width=1920 height=1080 memory=host-shared sid=…
[capenc] READY version=2 backend=native-nvenc codec=h265 chroma=yuv420
    bit_depth=8 range=limited matrix=bt709 primaries=bt709 transfer=bt709
    width=1920 height=1080 fps=30 … cursor=host capture=pipewire
    capture_zero_copy=false conversion_backend=gpu sid=…
```

The line is the shared `CaptureAdmissionNotice`. The Pier's
`wait_for_admitted_ready` accepts one before `READY`, checks it answers the
request it made (`check_request`: same preset and requested rate, a
permitted downgrade, no more than the requested rate, same codec, stated
cursor truth, same session), then expects `READY` at the admitted rate.
Without a notice a lower-rate `READY` is still a request conflict. The
served plan logs the decision, reason, copy path, assumed colour and
measured rate. An unavailable admission may be followed only by
`UNAVAILABLE` or capenc's exit; `READY` after it, or a second notice, is a
protocol error. When a session has an admission, the Pier's served-pipeline
label, priority and intent come from the admitted epoch
(`CapencHandle::admitted_epoch`), so a Speed request served as Auto is
labelled Auto.

The admitted rate never exceeds the request's (`AdmissionRequest::max_fps`):
an Auto request at 24 fps is offered, admitted, encoded and announced at 24
fps, which is not a degradation.

**Encode loop.** One tick per served frame interval takes the newest leased
frame only (anything older was already returned to Mutter with its damage
carried), copies it into the retained frame and releases the producer
buffer; Keel's `SubmissionGate` decides whether to encode: on change, on a
keyframe request, once more to flush NVENC, otherwise only at the one-second
keepalive. The retained frame is restaged into NVENC's current input slot
on every submission, as the NvFBC path does, so a still desktop can be
re-encoded as a keyframe on demand. Damage is the compositor's own:
GNOME 49 attaches `SPA_META_VideoDamage` regions to each buffer, which
capenc validates (`layout.rs`: none, an empty list, more than 32 regions or
a negative origin is unknown) and folds, together with the damage of
superseded frames, into `PendingCaptureDamage`. GNOME 40 sends none, so
every delivered frame there is full damage. With `qp-map` requested the
runner opens NVENC with that policy and engages a QP map fed from those
regions (`Encoder::enable_external_qp_map`; the eight-bit formats need no
readback because nothing is hashed). A picture is biased only when every
frame since the previous submission carried a region list
(`PendingCaptureDamage::is_authoritative`); full or unknown damage,
keepalives and IDRs get a neutral map. The stats line reports
`damage_source=metadata|unknown_full` and, when engaged, `qp_stats.*`.
Rocky Linux 10.2 / GNOME 49.4, Auto HEVC 1080p30, a 400×300 window
animating every 16 ms, host-side capenc numbers: `engaged=true`, every
submitted picture biased, mean dirty fraction 0.064 (the window's blocks);
the same desktop full-screen gave 1.0. A quiet stream is not a dead one; the epoch ends only when the
source closes, the producer renegotiates another size (a new epoch,
`wlp-recovery`), or the Pier closes stdin. Live frame-rate requests are held
to the admitted ceiling. Bitrate, VBV and keyframe policy come from the
shared contract through the existing NVENC adapter; the Xorg NvFBC and XShm
loops are untouched.

**Software encode.** A request for `encoder=software-h264` is served by the
separate `linux-wayland-software` composition; see
[Software encode](#software-encode-linux-wayland-software).

### Software encode (`linux-wayland-software`)

Code: `hosts/capenc/src/wayland/pipelines/software.rs` (the composition)
and `software_runner.rs` (the epoch); the shared pieces are
`arcen_media::video::SoftwareH264Session` (the one OpenH264 session the Xorg
MIT-SHM path also runs on), `SoftwareEncodeLoop` (Keel-gated encoding of
delivered pictures) and the Software contract
(`arcen_media::video::pipeline` `software`). Registered as the product path
`linux-wayland-software`.

A host without a usable NVENC — Intel or AMD graphics, a GPU without an
encoder, a VM with only a virtual GPU, or NVENC capacity exhausted — still
streams a Wayland desktop:

- **Selection.** The request still resolves to Auto or Speed with the
  shared preset rule; `encoder=software-h264` sends it to this composition.
  Nothing in it touches CUDA, EGL or NVENC, so it runs on a host with no
  NVIDIA driver at all.
- **Source.** Shared-memory BGRx/BGRA only (the proven route on GNOME 40
  and 49; DMA-BUF is never offered), at no more than the Software ceiling
  (30 fps), at the hand-off's exact size. Mutter reads its frame back into
  the buffer; capenc converts it on the CPU (`copy_path=host-copy`,
  `route=shm-cpu-convert`, `conversion_backend=cpu`).
- **Admission.** The requested contract's own shared admission given
  OpenH264, which applies the shared software rule: Auto is served as
  Software (H.264 4:2:0 eight-bit limited BT.709, up to 30 fps), Speed is
  visibly downgraded to it (`reason=software_fallback`), Grading and HDR
  never reach it (the Pier serves them as Auto on Wayland), and a stream
  larger than OpenH264's 1920x1200 is refused by name
  (`encoder_geometry_exceeded`). An embedded (compositor-drawn) cursor is
  carried as `cursor=host` (`BackendLimits::with_source_cursor`: the
  pixels already hold it); a metadata stream keeps the Deck's cursor.
- **Encode.** Each delivered picture is hashed once by Keel; only changed
  16x16 block rows are converted into the retained I420 planes; an
  unchanged picture costs no encode; Keel's idle cadence encodes on change,
  on a requested keyframe, and otherwise only at the one-second keepalive,
  re-encoding the retained picture. Live frame-rate and bitrate requests
  follow the Software contract (no forced IDR; bitrate between a quarter of
  the start rate and the ceiling), exactly as on Xorg.
- **Multi-monitor admission.** `admission-v1` with `encoder=software-h264`
  runs the shared OpenH264 probe at the hand-off's size, without capture —
  the probe the Xorg path runs too.

```text
[capenc] ADMISSION version=1 requested=auto requested_fps=30 decision=admit
    served=software codec=h264 fps=30 transform=sdr8-rgb colour=assumed
    cursor_mode=host capture=pipewire copy_path=host-copy cursor=composited …
[capenc] READY version=2 backend=openh264-sw-h264 codec=h264 chroma=yuv420
    bit_depth=8 range=limited matrix=bt709 … cursor=host capture=pipewire
    capture_zero_copy=false conversion_backend=cpu sid=…
```

On a host whose qualification proves no native encoder, the Pier asks for
software directly and creates each session's output fitted to OpenH264's
limits (see [Serving a Deck](#serving-a-deck-wlp-wiring)). On a native host
whose NVENC fails at spawn, the ladder falls to software when the output
fits, and otherwise closes with `the Wayland session's output is larger
than the software encoder's 1920x1200 and no hardware encoder is
available`. Vendor-neutral hardware encode (VA-API, Vulkan Video) is a
follow-up; see the swenc work package's VA-API design.

### Lab evidence (2026-10-17, GNOME 40 baseline host)

Rocky Linux 9.7, gnome-shell 40.10, PipeWire 1.0.1, GRID V100D-16Q driver
580.65.06, a throwaway user; `examples/wayland_session_harness --pipeline`
through PAM, the real launcher and `media::capenc::spawn` (debug Pier
build), with the GTK test client redrawing every 16 ms, then stopped:

| | Auto (H.264) | Speed request (HEVC) |
| --- | --- | --- |
| Admission | admit, Auto 30 fps | downgrade to Auto 30 fps, `copy_path_not_rate_qualified` |
| `READY` after spawn | 325 ms | 328 ms |
| Source rate while moving | 22.6 fps (Auto offer, 30 fps max) | 37.5 fps average, 41 max (Speed offer) |
| Encoded while moving | 30.0 fps, 147 kbps | 30.0 fps, 300 kbps, 8.4 superseded frames/s shed |
| Host copy (upload) | 0.83 ms average, 1.25 max | 0.84 ms average, 1.13 max |
| NVENC encode | 4.1 ms average (debug) | 4.2 ms average (debug) |
| Idle (client stopped, 15 s) | 0 activity submissions, 1.00 keepalive/s, 14 access units | same |
| Keyframe on demand while idle | 25 ms | 24 ms |
| Stream (`ffprobe`) | H.264 High, yuv420p, tv range, BT.709 ×3, 2 I + 619 P, decodes | HEVC Main, yuv420p, tv range, BT.709 ×3, 2 I + 619 P, decodes |
| Cursor | composited, visible in decoded frames; `cursor=host` | same |
| Readiness | first frame `bgrx8` 1920x1080 `host-shared`, import `host-copy`; capture route proven for the session (not GPU import) | same |

The release capenc under plain headless Mutter (`scripts/lab/wayland-capture-probe.sh`
with `ARCEN_LAB_CAPENC_RUN`) measured 0.82–0.95 ms upload and 2.5 ms
NVENC H.264 encode; a Speed request with `capture-rate=35` was admitted as
Speed at 35 fps (`capture_rate_below_contract`) and encoded 35–36 fps at
about 750 kbps with motion priority, while its source sustained 32 fps; an
idle desktop delivered exactly one frame on connect and none after.

Verdicts for this stack: **Auto** works end to end through capenc on the
real hand-off with a labelled host copy. **Speed** is not available: the
copying route has no qualified 60 fps rate (the source tops out near 35–41
fps), so a Speed request is visibly served as Auto. In the first harness run
of a new user, GNOME's welcome tour took focus and the RemoteDesktop input
check failed; later runs passed.

### Qualifying Speed on another stack

Speed needs, on the host being qualified: a sustained source rate at the
contract ceiling measured on the route that will serve it (the harness or
the capture probe on moving content), recorded as the host's
`capture-rate`; or a qualified zero-copy import. A newer GNOME (Mutter 49 on
the Rocky 10 host) is expected to offer DMA-BUF on the render node; that
route stays unqualified (`dmabuf_unqualified`) until a real producer buffer
is imported, synchronised and read back correctly, and it is `gpu-convert`,
so it needs a rate too. Then: run the Speed harness and capture probe there,
record the measured rate, upload/import and encode cost, and bounded
latency; decide where the Pier gets `capture-rate` from (an installation-time
qualification record); and verify on a Deck that `DecodeLatencyPolicy::SPEED`
and the one-frame VBV hold at 60 fps.

On Rocky Linux 10 / Mutter 49 (see its qualification) a 60 Hz virtual
monitor with a 60 fps offer delivers 36–41 fps whatever the driver,
resolution or memory type, so Speed is served as Auto; with a measured
`capture-rate=49` (llvmpipe interim stack) it was served as Speed at
49 fps. A 120 Hz virtual monitor with a 120 fps offer delivered 59.6 fps
(1080p) and 56.3 fps (4K, debug consumer) by host copy and about 60 fps by
DMA-BUF. capenc's EGL-image import (`cuGraphicsEGLRegisterImage`) fails
with `CUDA_ERROR_INVALID_VALUE` on those NVIDIA block-linear buffers; a GL
blit into a CUDA-registered texture reads them correctly.

## Wayland Grading and HDR (`wp-fidelity`)

Grading (HEVC 4:4:4 10-bit full-range BT.709) and HDR (HEVC 4:4:4 10-bit
BT.2020/PQ) on Wayland are two compositions of their own
(`hosts/capenc/src/wayland/pipelines/{grading,hdr}.rs`, run by
`pipelines/wide.rs`). They share nothing with the eight-bit Auto and Speed
routes, which are unchanged. Unlike Xorg, Wayland can serve real HDR, but
only on a compositor that has HDR screen sharing.

**What the compositor must give.** Mutter added HDR screen sharing in
GNOME 50 (mutter!4928). `RecordMonitor` offers `xRGB_210LE`, `xBGR_210LE`
and `RGBA_F16`, tagged BT.2020/SMPTE 2084, only while the monitor is in an
HDR (BT.2100) colour state. It never offers them in SDR. `RecordVirtual`
offers eight-bit formats only, and no released Mutter can put a virtual
monitor into the `bt2100` colour mode. Mutter 49 and older therefore have
no wide route, and the shared decision says so:
`arcen_media::video::compositor_fidelity::fidelity_route` returns
`compositor_lacks_hdr_screencast`, whose text cites mutter!4928.

**The route on stock Mutter ≥ 50: `force-hdr`.** For a Grading or HDR
session on a host whose record proves the route, the Pier starts the
session's compositor with `MUTTER_DEBUG_FORCE_HDR=1`:

- The agent receives `--fidelity-route force-hdr`, and
  `HeadlessStack::start` sets the variable in that one compositor's
  environment.
- Every colour device, virtual monitors included, then composes in
  BT.2020/PQ, with SDR content at a 203-nit reference white.
- `--debug-control` is never used, because it would export Mutter's debug
  toggles to session-user processes.
- An output colour-mode route (`colour-mode`) replaces this once a
  compositor offers BT.2100 on virtual monitors. The decision already models
  it.

The agent then records each output of the current generation again with
`RecordMonitor`:

- The Pier requests this with `OutputCommand::WideStreams { generation }`.
- The streams are reused within a generation and stopped on every
  reconfiguration.
- The request is refused for a stale generation or a session not on the
  route.
- The Pier hands capenc the wide node beside the ordinary one
  (`ARCEN_PIPEWIRE_WIDE_NODE`), together with the record's proven facts
  (`fidelity-route=`, `sdr-white=`, and
  `wide-proven=<format>@<memory>,…`, for example
  `xbgr2101010@dmabuf,rgba-f16@dmabuf`). A serving epoch offers only the
  proven formats.

| | Grading | HDR |
| --- | --- | --- |
| Capture | Wide `RecordMonitor` stream, offered `RGBA_F16`, `xBGR_210LE`, `xRGB_210LE`; DMA-BUF preferred | Wide `RecordMonitor` stream, offered `xBGR_210LE`, `xRGB_210LE`, `RGBA_F16`; DMA-BUF preferred |
| Source precision | F16 PQ (proven native-wide), or 10-bit PQ | 10-bit PQ, proven 1024/1024 |
| Copy boundary | DMA-BUF imported on the compositor's render node through EGL and read back to host memory as `GL_UNSIGNED_INT_2_10_10_10_REV` or `GL_HALF_FLOAT` (`wide_readback.rs`); `copy_path=host-copy` | Same |
| Conversion | Shared: PQ/BT.2020 to linear, 203-nit graphics white to SDR 1.0, BT.2020 to BT.709, then the SDR transfer. F16 uses `convert_rgba_f16_pq_to_packed_rgb10`; 10-bit uses `Rgb10Signal::PqBt2020ToSdrBt709` | None: PQ codes kept |
| Encoder | RGB→YUV into I444P16, NVENC HEVC Main 4:4:4 10, full range, BT.709 | RGB→YUV into I444P16, NVENC HEVC Main 4:4:4 10, full range, BT.2020/PQ/BT.2020 NCL |
| Deck | Ten-bit Metal layer, EDR off | Ten-bit Metal layer, PQ/EDR on |

**Admission never trusts a format.** Each epoch carries
`CaptureSourceEvidence` built by `wide_source_evidence`:

- Sample precision is native-wide only for the negotiated format in the
  negotiated memory, as a pair the record proved.
- The colour comes from the stream's SPA colorimetry (transfer 14 = PQ).
- HDR is active only on the route with PQ.
- `pq_graphics_white` must be authoritative and within ±5 nits of 203.

The shared `admit_grading` and `admit_hdr` then decide, using the new
`SourceTransform::HalfFloatPq` arms (`f16-pq-direct`, `f16-pq-to-sdr`).
Shared-memory wide frames are excluded because Mutter's shm screencast still
paints through an eight-bit path (50.6, 51.1, main). On the lab they carried
256 levels in a ten-bit container (wrong channel order) and garbage for F16.
Without that proof they stay `Unknown` and are refused. A shared-memory
copy keeps the producer's own channel order (`xRGB_210LE` or `xBGR_210LE`)
when it is staged. The DMA-BUF read-back always yields `XBGR2101010`
words.

**Qualification.** `arcen-pier wayland qualify` runs a fidelity session
after the output measurements (`session/wayland/qualify/fidelity.rs`). The
whole proof is host-side:

1. Route from the compositor release. Mutter < 50 records the gap and stops.
2. A session on `force-hdr`, then its wide stream.
3. capenc's own `wayland-ramp-client pq` shows a full-screen
   `XRGB2101010` picture (`qualification_picture_code`): a grey ramp, a
   white patch, and a patch of three distinct channel codes (900/520/140).
   It is tagged PQ/BT.2020 through `wp_color_manager_v1` and read back
   through the wide capture (`fidelity-probe`), once with the HDR offer
   (10-bit first) and once with the Grading offer (F16 first).
   - `precision_from_ramp` demands at least 90 % of the expected codes and
     a maximum error of one code; 256 or fewer distinct codes means
     upconverted eight-bit.
   - The channel patch must come back within one code per channel, which a
     grey ramp cannot show.
   - Each passing format and memory pair is recorded separately.
4. `wayland-ramp-client sdr`: where the composition puts SDR white.
5. A real Grading and a real HDR epoch admitted on those facts.

The record's `facts.fidelity` keeps the proof or the gap. Without the proof,
the Pier serves a fallback: HDR becomes Grading if Grading is proven,
otherwise Auto, with the gap as the reason.

**Measured (host-side, 2026-10-18;
[evidence](wayland-qualification/fedora-gnome50-51-fidelity-container.md)).**
GRID V100D, driver 580.105.08.
Fedora 44 and Fedora 45 headless GNOME Shell run in a podman container on
the Rocky 10 lab host; capenc runs on the host and consumes the container's
PipeWire. These are not product-supported stacks; they prove capture,
import, conversion and encode only.

- Mutter 50.6, `force-hdr`: the output colour state was BT.2020/PQ with a
  203-nit reference.
  - DMA-BUF `xBGR_210LE`: 1024/1024 codes, maximum error 0, import about
    6.6 ms.
  - DMA-BUF `RGBA_F16`: 986/1024, maximum error 1.
  - SDR white measured 202.9 nits (PQ code 594).
  - HDR epoch: READY BT.2020/PQ; SPS truth `rext 4:4:4 10-bit
    bt2020/pq/bt2020ncl full`; 30 fps encoded; import about 10 ms; staging
    3.4 ms; NVENC 5 ms.
  - Grading epoch (F16): SPS BT.709 4:4:4 10-bit full; 30 fps; import
    5.5 ms.
- Mutter 51.0 (Fedora 45), the same runs:
  - DMA-BUF `xBGR_210LE`: 1024/1024 codes, maximum error 0, import
    6.7–7.8 ms.
  - DMA-BUF `RGBA_F16`: 986/1024, maximum error 1.
  - SDR white measured 202.9 nits.
  - With an animated client, both epochs were admitted, encoded and
    checked:
    - HDR: `xBGR_210LE`, modifier `0x03000000004fe014`; READY and SPS truth
      `rext 4:4:4 10-bit bt2020/pq/bt2020ncl full`; 30–31 fps encoded;
      import 10.5 ms; staging 3.4 ms; NVENC 5.0 ms.
    - Grading: `RGBA_F16`, `f16-pq-to-sdr`; SPS truth `rext 4:4:4 10-bit
      bt709/bt709/bt709 full`; 30–31 fps; import 5.7–6.0 ms.
    - The client drew about 20 fps (`source_fps`). That is the client's own
      rate, not a compositor limit.
  - Mutter 50.6 gave the same figures within 0.5 ms.
- On both, the PQ-tagged picture was the measuring ramp. Shared-memory wide
  frames are refused as described above.
- Mutter 49.4 (the product stack, a dev record beside the installed
  service): `arcen-pier wayland qualify` records
  `fidelity.gap = compositor_lacks_hdr_screencast` and prints "Grading: no
  (Mutter 49.4 has no HDR screen sharing; it arrived in Mutter 50
  (mutter!4928))". Grading and HDR are then served visibly as Auto.

**Honest limits.**

- Grading through a PQ composition is near parity, not parity. An SDR
  ramp keeps 853 of 1024 codes through F16 PQ (445 through 10-bit PQ),
  where Xorg depth 30 keeps 1024. Full parity needs wide formats in the SDR
  colour mode upstream.
- A session opened for Auto and later resumed with a Grading or HDR request
  was not started on the route. Its agent refuses wide streams, and capenc
  fails visibly ("no wide format negotiated"). Restarting the session on the
  route is a follow-up.
- Product qualification of the full Pier→Deck path needs a GNOME ≥ 50 host
  with the same vGPU. The containers cannot run the Pier's logind session.

`MUTTER_DEBUG_FORCE_HDR` is a compositor debug switch. Using it in a
product session needs Release/Security and Shared/Architecture review.

## Detection evidence and the later shared contract

The Wayland names describe inventory and detection only. A future native
adapter may use a successful `WaylandOutputSource` snapshot as evidence while
implementing the separately reviewed shared
`arcen_outputs::OutputProvider`; it must not pass
`WaylandOutputCapabilities` directly to the shared admission gate.

| Wayland evidence | Later `arcen_outputs::OutputCapabilities` meaning |
| --- | --- |
| `enumerate_outputs` | Selection precondition only; it is not a shared capability. |
| `xdg_output_logical_regions` plus a coherent snapshot | Permits `signed_desktop_coordinates` only when the provider verifies that logical placement is authoritative. |
| `fractional_scale` plus an authoritative output association | Permits `fractional_scale`; merely observing `fractional-scale-v1` is insufficient. |
| `mutter_virtual_output: Implemented` | May support `surface: Virtual` and `headless_capable: true`; the native provider must still prove the lifecycle and teardown promises. |
| Unknown, unavailable, or detected-but-unimplemented virtual-output state | No shared capability and no provider selection. |

The snapshot's mode, transform, scale, region, and teardown evidence may later
support the remaining semantic fields (`exact_modes`, `per_region_rotation`,
`persistent_dedicated_desktop`, and rollback), but those are provider promises
that require native implementation and verification. They are not inferred
from detection flags alone.

## Follow-up boundary

Runtime enablement requires separately reviewed native adapters and Linux
integration evidence. The `zbus` D-Bus client (MIT, pure Rust, only in
`wayland-provider` builds) awaits Release/Security dependency review; any
further Wayland, portal or libei dependency needs the same. Launcher and
shared selection changes also need Linux Host and Shared/Architecture
review. Until then, Xorg remains the production default.
