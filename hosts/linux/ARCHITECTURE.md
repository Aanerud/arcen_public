# hosts/linux — `arcen-pier-linux` (Arcen Pier, Linux)

**Delivery:** Arcen Pier on Linux. Product surface. The single `arcen-pier`
binary dispatches capenc, audiocap, input-helper, session-launcher, and
session-agent subcommands.

## What this is

The native Rust control plane for a Linux workstation host: a QUIC/TLS 1.3 server, PAM/SSSD
authentication, per-user graphical session management, input injection (uinput), display
management, and supervision of the `arcen-capenc` capture+encode subprocess. Replaces the
old Python `server/*.py` on the Linux path. No Python, no ffmpeg in the app path.

## How it's grounded

- Lessons encoded (the important ones):
  - **Dedicated Xorg per session on its own GPU head + logind seat activation** is the
    correct per-user model (matches the established per-session launcher model:
    launch a dedicated Xorg config with `OutputClass` + `ActivateSessionOnSeat`). The
    "gnome-session on the shared root-owned `:0`" path is **wrong** — jc's logind session is
    `Active=no`, mutter fails `meta_later_add: assertion 'display' failed`, GNOME crash-loops
    ("Oh no…"). Proven live on pier-linux.example.internal: a dedicated `:10` on head DVI-D-1 streamed a real
    per-user desktop. **This is the top Linux Pier follow-up** — earlier analysis captured it as the
    `LINUX_SESSION_MODEL_SPEC` finding, not fully coded.
  - **GPU head ↔ display ↔ capenc output-index rule:** up to 4 concurrent sessions/screens,
    each pinned to its own head (DVI-D-0..3); capenc `--monitor N` selects output index N-1.
    See `hosts/capenc/ARCHITECTURE.md`.
  - **Identity drop is ordered:** `setgroups → setgid → setuid`, then PDEATHSIG re-armed.
    Credentials travel over a private stdin pipe as `Zeroizing` bytes; PAM owned by the root
    session-launcher (`pam-client2`), validated via loginctl + `/proc/self/cgroup`.
  - **Resilient headless Xorg:** BusID-detected config in `/run`, never touches
    `/etc/X11/xorg.conf`.
  - Reasoned WebSocket close frames (UTF-8-safe 120-byte truncation) for
    `persistent_session_busy` / `graphical_session_setup` instead of bare `Close(None)`.

## Rules — what it must be (invariants)

1. Depends on `arcen-capenc`, `arcen-input`, `arcen-media`, `arcen-protocol`,
   `arcen-session`, and `arcen-telemetry`; spawns `current_exe() capenc` as a
   subprocess. Never depends on the Windows Pier or the gateway.
2. Linux-only deps (`pam-client2`, `evdev`, `nix`, `x11rb`) stay behind
   `cfg(target_os = "linux")` so protocol/transport unit tests still run on the macOS dev box.
3. `unsafe` allowed (~49 sites: uinput, nvctrl, identity syscalls) but scoped.
4. Must not disturb other co-resident remote-desktop services on the box; own UDP port `18444`, own
   install prefix `/opt/arcen/bin/`, own systemd unit `arcen-pier.service`.
5. QUIC/TLS via Quinn and rustls. Tungstenite remains only as the bounded
   message-framing codec over the QUIC stream.
6. Capenc's stable `[capenc] enc_fps=...` pipeline line is INFO-visible; routine
   child diagnostics remain DEBUG and explicit child warnings remain WARN.
7. `server_hello`, cursor authority, video headers, input geometry, and quality
   acknowledgement use one `ResolvedMediaPlan` parsed from capenc READY. Child launch alone is never
   treated as capture/encoder readiness.
8. Packaged logging has one authoritative sink:
   `/var/log/arcen/arcen-pier.log`. Logrotate renames/creates it and `SIGHUP`
   reopens the file, reloads the last-good filter, and runs shared age cleanup.
   `Xorg.log` remains outside this retention policy.
9. Mandatory direct QUIC on UDP `:18444` uses the operator-managed PEM. The
   process validates and reloads material but never
   issues, renews, rotates, fetches, or persists trust. There is no mTLS,
   0-RTT, OIDC, Span, or commercial licensing dependency.
10. Linux `auto` resolves NVENC then source-built OpenH264 only after typed
    native unavailability. Software limits are selected before display mutation;
    malformed READY or runtime failure closes rather than switching codec.

## Interfaces / boundaries

- **Consumes:** `arcen-input`, `arcen-media`, `arcen-protocol`; `arcen-session` restore-lease primitives;
  embedded `arcen-capenc` through a Pier subprocess (Annex-B on stdout); PAM/logind; X server(s);
  uinput.
- **Exposes:** QUIC on UDP `:18444`.

## Module map (from the proven source)

- `main.rs` / `lib.rs` / `cli.rs` — entry, crate root, CLI (host, port, display, flags).
- `logging/` — canonical `logging.level`/one-release legacy `logging.verbosity`
  resolution (`arcen-session::pier_config::LoggingConfig`), the shared
  `arcen-observability` runtime (canonical JSON Lines file, optional dev
  console, `ARCEN_LOG` refinement, bounded shutdown flush, `SIGHUP` reopen via
  `ObservabilityHandle`), and recognized-archive cleanup.
- `observability.rs` — `SessionHealth`: per-session host/client QoS
  hysteresis (`arcen-telemetry::QosTargets`), `HealthStatsMsg` construction.
- `netinfo.rs` — bounded `/sys/class/net` + `/proc/net/route` network probe
  (ethernet/wifi/vpn/loopback/other classification, link Mbps/MTU; no raw
  SSID).
- `eventlog.rs` — journald/syslog lifecycle sink (see "Native lifecycle events" below).
- `support_bundle.rs` — service-independent, bounded ZIP collection over strict
  log, systemd, runtime, command, and sanitized lifecycle-event allowlists.
- `net/` — `server.rs` (mandatory QUIC accept loop and reasoned closes), `quic.rs`,
  `tls.rs`, `mod.rs`.
- `session_admission.rs` — capacity-one physical session admission and the
  bounded direct-reconnect hold for the desktop/input/encoder plane.
- `session/` — `launcher.rs`, `agent.rs`, `lifecycle.rs`, `resume.rs`, `identity.rs` (uid/gid
  drop), `timezone.rs` (zoneinfo validation), `auth.rs` (PAM), `handshake.rs`,
  `client.rs`, `audio.rs`, `mod.rs`.
- `media/` — `capenc.rs` (bounded READY startup + supervision), `annexb.rs` (AU split),
  `audio.rs`, `multi_capenc.rs`, and `encoder_admission.rs`. Aggregate admission
  reuses the exact multi-capenc planner to build bounded hardware/software
  candidates, preserves full-color regions on NVENC, binds probes to the
  explicit Xorg `DISPLAY` plus committed `DFP-N`/dense-output tuple, and
  delegates measurement to an injectable adapter with explicit thresholds
  rather than assuming a vendor session count. The production adapter starts
  finite `admission-v1` children for every exact candidate binding
  concurrently, records aggregate/per-region latency, delivery, queue, and
  fairness telemetry, and applies only `selected_specs`; an all-candidate
  rejection occurs before any streaming supervisor starts.
- `input/` — `uinput.rs` (absolute/relative pointer device plus the separate
  `"Arcen Virtual Tablet Pen"` tablet-tool device), `pen.rs` (portable,
  `evdev`-free pure pen axis mapping and idempotent tool/button state
  machine), `region_adapter.rs` (a thin adapter over the shared
  `arcen_input::RegionInputPipeline`; it contributes only the final-boundary
  Xorg virtual-raster `RegionPointMapper`), `eis.rs`
  (capability-gated, pure libei/EIS region model and provider seam; no native
  adapter), `keymap.rs`, `mod.rs`. See "Region-scoped input" and "Typed
  pen/tablet input" below.
- `clipboard.rs` / `clipboard/native.rs` — trusted parent relay/supervision and
  the user-context x11rb XFixes/ICCCM selection agent.
- `display/` — `nvctrl.rs`, `topology.rs`, and `wayland.rs` (host-local
  `wl_output`/`xdg-output`/fractional-scale model and fail-closed provider
  detection; not selected by the launcher). `topology.rs` owns the Xorg/RandR
  head inventory, mode matching, and framebuffer ceiling; the rotation-aware
  footprint math, edge-aware mixed-scale placement, origin policy, and
  plan→region-set construction it uses live in the shared
  `arcen_media::topology_placement` module. This host passes
  `TransformConvention::NativeNeedsTransform` (RandR reports native
  pre-rotation mode extents) and `OriginPolicy::TranslateToNonNegative` (an
  Xorg screen has no negative space).
- `session/launcher.rs` and `session/agent.rs` — multicall subcommands of
  `arcen-pier`; the launcher owns PAM and dedicated Xorg.
- Fused helper modes: `arcen-pier audiocap` (from `server/audiocap`) and
  `arcen-pier input-helper` (from `server/input_helper`).
- The first Wayland provider tranche is API/model-only. Dedicated Xorg remains
  the default and only wired output/input session path. See
  [`../../docs/architecture/linux-wayland-provider.md`](../../docs/architecture/linux-wayland-provider.md).
- Audiocap monitor inactivity may mean PulseAudio suspended an idle
  `auto_null.monitor`. Pier preserves the in-flight 3,840-byte read and reports
  the idle period without killing the helper; only EOF/read/liveness failure
  enters bounded reap-and-restart. A resumed stream records an honest
  inter-chunk capture gap.
- PAM sessions validate the Deck's early `AuthResponse.session_log_id`; legacy
  no-auth remains server-first and uses a host fallback. The same diagnostic
  `sid` follows the Pier, persistent launcher/agent forwarding, capenc, and
  audiocap; reconnect logs `previous_sid` without restarting the desktop.
- PAM sessions validate `AuthResponse.displays_mode` before launching the
  desktop. Linux accepts legacy empty mode, `single_primary`, `windowed`, and
  single-monitor `match_layout`. `single_primary` and `match_layout` pin the
  stream to the negotiated client monitor size; only `windowed` advertises and
  accepts live `display_update` resize. Multi-monitor Match-My-Layout is
  degraded loudly to the primary client monitor until Linux grows more than one
  mirrored host display.
- `--disclaimer` prepares the selected
  `--disclaimer-dir`/`--disclaimer-locale` text file once before `net::serve`
  (defaults: `/etc/arcen/disclaimers` and `en_US`). It requires PAM mode and
  rejects missing, empty, invalid UTF-8, oversized, or unsafe locale input.
  The exact text is attached to `AuthRequest`; its lowercase SHA-256
  acknowledgment is checked before PAM validation, PAM semaphore acquisition,
  or privileged launcher entry.
- Acceptance evidence is emitted only after the launcher reports successful OS
  authentication and contains only locale, digest, host time, correlation ID,
  and success. It is standalone operational evidence, not a
  `SessionGrantClaims` v1 binding.

## Portable software capture and encoding

`encoder=software-h264` skips NVIDIA initialization. `arcen-capenc` connects as
the authenticated dedicated-Xorg user, selects the configured connected XRandR
output, validates little-endian 24-depth/32-bpp BGRX layout, and uses MIT-SHM
1.2 when available. XDamage drives activity; a bounded full GetImage/polling
mode is warned once when native extensions are unavailable. A modeset destroys
and recreates borrowed X11/SHM state and refuses hidden geometry change.

Every emitted software frame is a full checked BGRA capture converted directly
to I420 in shared safe Rust and encoded by the shared source-built OpenH264
wrapper. READY follows the first successful encoded access unit. The software
cap is even H.264/YUV420 local-cursor geometry through 1920x1080 at 30 fps.
Idle cadence remains one-second keepalive. Selective retained conversion is not
enabled.

The implementation has offline unit/CI coverage but no claimed physical
non-NVIDIA fallback, Deck decode, 1080p30 performance/allocation, modeset,
one-hour soak, or restore result. Those remain release gates in
[`../../docs/architecture/media-plan-resolution.md`](../../docs/architecture/media-plan-resolution.md).

## Relative input and cursor capture

The attachment dispatcher owns one shared `InputSequenceTracker` across
keyboard, absolute/relative motion, button, and wheel messages. The dedicated
session uinput device advertises `REL_X`/`REL_Y`; each relative motion is one
two-axis synchronized batch, and Relative button/wheel messages omit
`ABS_X`/`ABS_Y`.

Authenticated cursor preference is fixed before capenc starts and is part of the
resume topology binding. NvFBC receives `bWithCursor` in its capture-session
parameters and emits the resolved `cursor=local|host` in READY after the first
encoded frame. Pier rejects mismatched or missing READY metadata, advertises Host
only for the proven native NvFBC backend, and sends the cursor result before
media. No raster compositor or X11 cursor-shape dependency is introduced.

## Region-scoped input

Committed Match My Layout attachments require input protocol v4 and mutual
`input_capabilities.region_input = available`. Linux advertises that capability
only when the attachment has successfully constructed its shared region
adapter and native uinput backend; a client lacking the same v4 capability is
disconnected during `ClientHello`.

`RegionPointer*` and `RegionPenEvent` messages remain in region-local logical
coordinates through shared decoding and `RegionInputState` validation inside
`arcen_input::RegionInputPipeline`. Only `input/region_adapter.rs`'s
`RegionPointMapper`, immediately before uinput emission, turns the resulting
applied pixel index into the dedicated Xorg virtual raster axes. Legacy `mouse_move`, `mouse_button`,
`mouse_move_relative`, `mouse_scroll`, and `pen_event` DTOs are rejected
whenever that adapter is active; keyboard/reset messages remain separate.

## Typed pen/tablet input

Full design: [`../../docs/architecture/pen-tablet-input.md`](../../docs/architecture/pen-tablet-input.md).
Operator validation: [`../../docs/operations/pen-tablet-input.md`](../../docs/operations/pen-tablet-input.md).

`input/uinput.rs::build_tablet_device` creates a **separate** virtual
`"Arcen Virtual Tablet Pen"` device (not merged into the existing
absolute/relative mouse `uinput` device, so `libinput`/Xorg classify each by
its own complete capability set), advertising `ABS_X`/`ABS_Y` on the shared
fixed device raster, `ABS_PRESSURE` on the documented inclusive 13-bit
`0..=8191` Linux range (`input/pen.rs::PRESSURE_MAX_13BIT`, matching modern
Wacom Linux-driver magnitude without overclaiming vendor resolution),
whole-degree `ABS_TILT_X`/`ABS_TILT_Y` (`-90..=90`), tool/proximity via
`BTN_TOOL_PEN`/`BTN_TOOL_RUBBER`, tip contact via `BTN_TOUCH`, and two
barrel buttons via `BTN_STYLUS`/`BTN_STYLUS2`. **No rotation axis is
advertised**: `pen_rotation` is always reported `Unavailable` because no
target here has proven the kernel/libinput stack recognizes a chosen evdev
axis as tablet rotation.

`input/pen.rs::plan_pen_edges` emits ordered, idempotent `EV_KEY` edges
(tool bit before touch/buttons entering proximity, touch/buttons before the
tool bit leaving proximity) from one already-`PenEventMsg::validate()`d
sample, and treats any out-of-proximity sample as fully released regardless
of stale `touching`/`buttons` bits a peer still carries. Every held code is
released on reset/drop. The tablet-tool device is probed/created before
`ServerHello` is built (`InputController::pen_available`), so this host
advertises pen/pressure/tilt/eraser/proximity as `Available` only once the
runtime backend actually established the device; a pen-creation failure
alone still leaves mouse/keyboard available. No Wacom driver is installed or
required on this Linux host — `libinput`/Xorg see an ordinary kernel
tablet-tool device on the dedicated per-session Xorg target described above.
Tablet touch is not transported; only one active tool is supported.

## Clipboard redirection

No-auth/shared-display sessions advertise Disabled. Only an authenticated
dedicated-Xorg `SessionLease` can negotiate exact clipboard v1. After
ClientHello, Pier launches `arcen-pier session-agent --clipboard-agent`
through the lease's validated identity/environment with stdin/stdout pipes,
stderr-only logs, parent-death/process-group teardown, and bounded raw WebSocket
framing. READY binds pid/uid/username/DISPLAY and XFixes 5+.

Root Pier owns policy, Deck framing, and validation. The user child owns one
hidden X11 window, CLIPBOARD/TARGETS/TIMESTAMP/text/image/INCR atoms, XFixes
owner notifications, and one latest remote selection. It prefers UTF-8 text
targets, supports Latin-1 STRING conversion, serves text aliases or image/png,
and returns NONE for unsupported targets. ICCCM INCR send/receive uses at most
1 MiB properties, one transfer in each direction, delete handshakes, checked
declared sizes, and five-second progress deadlines.

The child exits and releases a remotely owned selection on every Deck
disconnect, including resumable detach; the persistent desktop and its restore
leases may remain. A resumed attachment re-negotiates and starts fresh bounded
clipboard state without altering display/timezone/media ownership.
- **Time-zone redirection is opt-in and disabled by default.**
  `--timezone-redirection` enables it for PAM-authenticated desktops;
  `--no-timezone-redirection` disables it with last-option-wins semantics, and
  `--zoneinfo-root` selects the trusted database (default
  `/usr/share/zoneinfo`). The host accepts only validated IANA identifiers whose
  canonical zoneinfo target is a regular file contained by that root; the
  `posix` and `right` alternate trees are rejected. No-auth mode is inert.
- The validated `TZ` belongs to the dedicated authenticated desktop's user
  process tree (PAM environment, session agent, desktop, capture, and per-user
  activation environment). It persists with that `DesktopSession` across
  disconnect/reconnect; a reconnect mismatch is warning-only and retains the
  running desktop's value. Process teardown completes the in-memory restore
  lease; Linux intentionally writes no durable time-zone journal.
- Redirection never changes `/etc/localtime`, invokes `timedatectl`, or sets a
  machine-wide systemd environment. The agent forwards `TZ` only through
  `dbus-update-activation-environment --systemd` for the authenticated user's
  D-Bus activation environment and user manager. The dedicated PAM desktop owns
  that per-user activation state; PAM/logind teardown and the user-manager
  lifecycle are its restoration boundary. The D-Bus update API cannot restore
  an exactly absent variable, so Arcen does not fake restoration with an empty
  `TZ`. A lingering user manager or application can retain/cache the value after
  desktop teardown and requires an operator-managed user-manager or application
  restart. This mechanism makes no universal-application or certification
  claim.

## Direct SessionAutoReconnect

Linux Pier supports explicit, credential-free resume over direct QUIC. The configured
`--reconnect-window-secs` defaults to 1200 and is validated in the inclusive
shared range `0..=7200`; zero disables advertisement, grant issuance, and the
detached grace period, so attachment/media cleanup and display restoration
begin immediately. The existing PAM-authenticated persistent-desktop cache is
separate and still requires full authentication. PAM is always used for the
initial connection. A grant is issued only when the Deck
opts in with a valid holder nonce and PAM, disclaimer evidence, the dedicated
Xorg launcher/agent, active same-uid logind session, stable TLS SPKI identity,
client topology, display ownership, and timezone lease are all fixed. Legacy
Decks receive no grant and keep the previous non-resume behavior.

`SessionRegistry` owns one process-local resume authority and one current
session slot. Startup creates a fresh system-RNG 256-bit ring HMAC-SHA-256 key;
the key is never derived, persisted, formatted, or logged and is zeroized on
drop. Claims use `arcen-identity` canonical domain-separated bytes and bind the
TLS SPKI host identity, Linux uid + logind session, active host session,
Deck-holder nonce, grant generation/nonce, disclaimer evidence, and expiry.
The registry verifies all bindings and atomically compare-and-rotates the
shared `DirectResumeSlot`; malformed, expired, replayed, or mismatched
`method=resume` requests never enter PAM. Each attempt has a new Session Log ID
and resume logs link only the bounded `previous_sid`.

While an opted-in attachment is streaming, its one sender loop refreshes the
single slot every `W/2` and sends the `2W` successor as an ordered in-band
`AuthResult` before more media. Rotation precedes send and has no acknowledgment:
lost delivery makes Deck's old grant a replay after loss. A known successor
send failure enters terminal drain immediately; otherwise an authenticated,
unexpired exact predecessor on the detached slot is rejected as `replayed` and triggers that same
drain. Neither path reopens the predecessor or discloses the successor. Final
cleanup restores/releases the retained display resource once, after which
normal visible authentication can proceed. Generation/signing failure also
enters terminal drain. Legacy attachments never own this interval.

Unexpected EOF, reset, liveness timeout, or transport write failure stops capenc and
audiocap, closes the attachment's media queues, and resets uinput. Only after
that attachment cleanup does ownership of the existing
`HeldDisplayResources` (`MetaModeGuard` plus single-session permit) transfer to
`SessionRegistry`. The
registry retains the launcher, user agent, dedicated Xorg, desktop/apps,
display mode, and timezone restore lease while one monotonic
generation-tagged deadline, anchored at unexpected loss, is authoritative;
failed resume transports preserve it. Resume rechecks launcher/agent
liveness and active same-uid logind ownership, takes the same held display
resources, sends the already-rotated successor grant, respawns media on the
same display, and explicitly requests a fresh IDR before frames are accepted.
All logind observations are kill-on-drop subprocesses with an explicit short
timeout, and active opted-in writer sends are bounded to
`min(existing timeout, W/2)` (at least one millisecond). It does not reapply
display or timezone state.

Explicit WebSocket Close, deadline expiry, terminal TLS/topology/native
identity mismatch, launcher/agent death, Pier shutdown, successor delivery
failure, or authenticated exact-predecessor replay on a detached slot enters one idempotent drain.
Resume is revoked first; the owner stops input/media,
restores/releases display ownership once, then shuts down the
launcher/agent/Xorg so the timezone lease can complete, and removes the
registry entry only after cleanup. `SessionRegistry::shutdown` is the fallback
owner if a task is aborted: terminal desktop cleanup runs in a registry-owned
task, and shutdown joins every outstanding task before completing. A Pier
crash remains non-resumable and relies on
the existing PDEATHSIG/drop-recovery boundaries; no resume key or slot is
durable.

This surface adds no Gateway/Span, OIDC, entitlement, licensing, mTLS, 0-RTT,
host-address binding, persistence, network fetch, or protocol-version bump. The
owning contract and complete ordering are in
[`../../docs/architecture/session-auto-reconnect.md`](../../docs/architecture/session-auto-reconnect.md).
Graduation requires Shared/Architecture, Release/Security, macOS Client,
Windows Host, and Linux Host review.

## Deskside physical-console privacy

Deskside is disabled by default and is accepted only with PAM, the root session
launcher, uinput, an explicit physical console DISPLAY/Xauthority, unique
`/dev/input/by-id` keyboard/pointer pins, and unique console output DRM/EDID
hashes. Configuration also pins the expected local console UID and a normalized
positive DMI system/chassis hash; CPUID must report no hypervisor. Configuration
must prove the console display and output names are
distinct from the dedicated capture `DISPLAY=:N` and `session_gpu_head`.

After PAM/logind creation and dedicated-Xorg startup, but before the user agent,
capture, input, or capability exchange, the root launcher proves the streaming
session is the expected `Remote=yes` PAM session, independently enumerates
exactly one active local `Remote=no` seat0 session matching the configured UID
and console DISPLAY, verifies that UID owns the mode-0600 console Xauthority,
validates positive DMI/chassis evidence, and resolves every input
pin to a unique evdev event node, positively classifies a physical bus and
keyboard/pointer capabilities, rejects Arcen's BUS_VIRTUAL uinput device, and
refuses any other relevant physical evdev device that is not pinned. It then
grabs every device with `EVIOCGRAB`; any failure ungrabs every acquired device.
The launcher re-enumerates the complete relevant evdev inventory every 250 ms;
a new, replaced, virtual, or unpinned device terminally drains the session
instead of continuing with an incomplete grab set. Every key/button or relative
axis device is relevant; absolute-axis classes are unsupported and refuse.

The launcher separately matches every connected physical DRM connector and EDID
hash to the configured console outputs, snapshots bounded xrandr geometry and
DPMS state, writes `/run/arcen/deskside-recovery.json` as a root-owned mode-0600
non-symlink file under a mode-0700 directory, and runs bounded console-only
xrandr/DPMS plans. No command targets the dedicated capture DISPLAY. The root
guard, evdev descriptors, dedicated Xorg, NvControl mode guard, desktop, and
launcher remain alive through direct reconnect.
The same supervisor continuously rehashes every DRM/EDID pin, rejects connector
add/remove/replacement, reparses the complete console xrandr inventory, requires
every output to remain mode-off, and requires DPMS to remain off.
Every xrandr, xset, and loginctl child has a bounded timeout; timeout kills and
reaps the child while retaining recovery state.

Broker pipe EOF, explicit drain, reconnect expiry, and normal shutdown stop the
session agent first, restore console outputs/DPMS, release every evdev grab, and
only then tear down dedicated Xorg/PAM. Launcher death closes descriptors in the
kernel; service startup replays a retained display journal before listening.
Restore failure retains the journal and still releases input for operator
recovery.
Journal-stage I/O errors never short-circuit physical restore. Live guard drop
schedules display-first asynchronous cleanup before releasing evdev ownership;
startup recovery remains the process-death fallback.

Unknown buses/connectors, missing EDID, unpinned hot-plug devices, overlap, and
journal permission/corruption all fail closed. Pen/tablet classes are unsupported
and cannot be requested. Physical evdev, DPMS, hot-plug, sleep/resume, GPU reset,
and forced-crash behavior remain mandatory physical-lab release gates.

## Log rotation and runtime verbosity

The complete shared/common and Linux platform schema is documented in
[`../../docs/operations/pier-configuration.md`](../../docs/operations/pier-configuration.md).

Packaged service mode loads the unified `/etc/arcen/pier.json`; its Linux
platform section supplies the fixed `/var/log/arcen/arcen-pier.log` path. The
common logging section's canonical field is `logging.level`
(`OperationalProfile` discriminant 0–3); the packaged/production default is
`Level0` (`Critical`, i.e. lifecycle/critical-only). The legacy numeric
`logging.verbosity` (`0..=3`) is accepted as a one-release migration
(`arcen-session::pier_config::LoggingConfig::resolved_profile`) and is
mutually exclusive with `logging.level` — configuring both is rejected. The
current `pier-linux.example.internal` development deployment overrides this to `Level2`
(`Info`) for live verification; that override is **not** baked into the
packaged default and must be set explicitly per-deployment. Retention days is
unchanged and still normalized to 7–100 days; the packaged logrotate policy
still owns the 32 MiB/daily rotation trigger. The canonical file is JSON
Lines (one `ValidatedLifecycleEvent`-or-log-record JSON object per line).
`ARCEN_LOG` remains the highest-priority fine-grained filter.

On `SIGHUP`, one coordinator independently attempts logging reopen/Pier-config
reload (through `ObservabilityHandle`) and TLS PEM validation/reload. Either
operation can succeed when the other fails; bounded deduplicated reporting
retains each subsystem's last-good state. A valid replacement atomically swaps
the rustls resolver, so established sessions continue. Standalone mode
preserves the previous daily rolling file plus stderr behavior and
`ARCEN_LOG_DIR`. Every successful reload re-emits `EFFECTIVE_PROFILE` (1805)
so the active profile and how it was resolved (`cli_override` /
`config_level` / `config_legacy_verbosity` / `production_default`) is always
observable without inferring it from output verbosity.

## TLS lifecycle

Before `TcpListener::bind`, readiness logging, or `SERVICE_START`, Pier opens
each PEM component relative to held directory descriptors with `openat` and
`O_NOFOLLOW`, then reads only the opened descriptor. It rejects traversal,
symlinks, non-regular files, changed-during-read snapshots, untrusted owners,
oversized material (1 MiB certificate, 128 KiB key), and insecure modes. The
key must be exactly `0600`; the certificate must be `0644` or stricter. Temporary
key-file bytes are zeroized.

The shared `arcen-transport` contract requires exactly one PKCS#8, PKCS#1, or
SEC1 key, certificate/key agreement, current validity, SAN presence, compatible
serverAuth EKU and digitalSignature usage, and admitted RSA/P-256/P-384/Ed25519
strength. Repeatable expected DNS/IP SANs are exact additional requirements.
The shipped product requires TLS 1.3, exposes the three rustls/ring TLS 1.3
suites, and uses a 30-day expiry warning.
CN-only legacy certificates therefore fail closed.

The reloadable resolver keeps active sessions on their negotiated key while new
handshakes use the validated replacement. A failed reload retains the last good
key. Once that key expires, the resolver refuses new handshakes and emits one
deduplicated `TLS_CERTIFICATE_EXPIRED` record. IDs 1400–1404 report only bounded
certificate/SPKI hashes, key class/size, expiry, source/component, warning days,
or a closed reason class—never paths, SANs, subject/issuer, raw errors, or key
metadata. Support Bundle continues to record logical TLS omissions and never
opens, stats, or hashes PEM.

Packaging installs `arcen-new-host-cert` for SMB generate-if-missing and explicit
operator renewal. Enterprise administrators may install their own pair.
Same-key renewal is:

```sh
sudo /opt/arcen/bin/arcen-new-host-cert --renew --directory /root/arcen/tls
sudo systemctl reload arcen-pier
```

`--new-key` is an explicit trust-changing reissue. The running service never
invokes OpenSSL or the helper.

## Native lifecycle events

Alongside the file/stderr `tracing` streams above, `arcen-pier` emits the
shared `arcen-telemetry` v1 lifecycle vocabulary (`ValidatedLifecycleEvent`,
IDs 1000–1806, see `shared/telemetry/src/lifecycle.rs`) as additive,
best-effort native records through `eventlog.rs`'s `LifecycleEmitter`, routed
through the shared lifecycle bridge's top-level `sid`/`user`/`host`/
`peer_addr` context (never nested inside the event's field payload).
`LifecycleEmitter::emit` always sets `user: None` and `peer_addr: None` —
Linux's native records never carry the authenticated username, uid, or peer
address, by design (a stricter posture than the bridge's contract permits,
not a gap). Emission never blocks or changes an auth/session/display outcome:
every adapter accepts only `ValidatedLifecycleEvent`, so schema/category/
outcome are already proven before any socket write. No username, uid, peer
address, raw PAM/child error, or credential material ever reaches a native
record — only the existing `session_log_id` correlation and schema-approved
safe fields (`stage`, `reason_class`, counts, health/network facts, etc.).

- **Delivery order:** a cached nonblocking Unix datagram to the systemd
  native journal protocol socket (`/run/systemd/journal/socket`), then a
  cached nonblocking Unix datagram to `/dev/log` (RFC 3164-style,
  `SYSLOG_IDENTIFIER`/tag `arcen-pier`), then exactly one structured
  `tracing` fallback report if both are unavailable or would block. No
  `libsystemd`/`sd-journal` linkage and no new logging dependency: delivery
  uses only `std::os::unix::net::UnixDatagram`, bridged behind the shared
  `arcen-observability` `BoundedSink`/runtime sink trait so counted delivery
  loss surfaces without recursive logging, and the bounded
  `query_recent_events` seam consumed by Support Bundle shells out only to the
  `journalctl` binary already present on a systemd host, or
  scans the approved conventional syslog files as a fallback.
- **Wired events:** `SERVICE_START`/`STOP`/`FAILED` around `net::serve`'s
  bind/shutdown; `SESSION_AUTH_OK`/`FAIL` around the broker's PAM/logind
  readiness boundary in `net::server::authenticate_if_required` and
  `run_ws`; `SESSION_STREAM_START` once capenc/display/input/audio setup and
  `server_hello` all succeed; a typed `SessionEndReason` classifies the
  frame-relay teardown into `SESSION_END` (clean client close) or
  `SESSION_INTERRUPTED` (liveness/transport/media/writer failure) instead of
  discarding which task ended first; `DISPLAY_ARMED`/`RESTORED`/
  `RESTORE_FAILED`/`WATCHDOG_RESTORE` are emitted from `MetaModeGuard` itself
  (`display/nvctrl.rs`), which now carries its own `LifecycleEmitter` and
  `CorrelationId` across the explicit-restore and `Drop`-triggered fallback
  paths — the `Drop` fallback is this crate's substitute for a separate
  watchdog process, since Linux has no persistent recovery journal file.
- Existing text logs, `SIGHUP` reload, and rotation/retention behavior are
  unchanged; native lifecycle records are additive.

## Health telemetry

`net::server.rs` runs a per-session `observability::SessionHealth` engine
(hysteresis against `arcen-session::pier_config::LoggingConfig::qos_targets`,
`arcen-telemetry::QosTargets`) alongside the existing per-session health tick:

- **Host-delivery health** is observed from real counters —
  `FrameQueue::frames_sent`/`frames_dropped`/`bytes_sent` and summed
  `InputStats` — never a fabricated value.
  `HealthStatsMsg` (real `HostCounters`) is broadcast to the client every
  tick; the host in turn consumes `HealthPingMsg::client_telemetry`
  (`ClientTelemetrySnapshotMsg`/`ClientQosSampleMsg`, PR3) so
  **client-experience health** reflects what the client actually observed,
  not just what the host sent. An old/legacy client that never reports
  telemetry leaves `SessionHealth::client()` as absent — treated as
  **unavailable**, never a stand-in healthy zero.
  `last_input_sequence`/`last_input_type` are not currently tracked by
  `InputStats` and are reported as `0`/empty placeholders in `HostCounters`
  pending an `InputStats` extension (a documented, scoped gap, not a
  fabricated fact).
- **Overall health** combines host and client state via the shared
  `QosTargets`/hysteresis rules and emits `HEALTH_OK`/`HEALTH_DEGRADED`/
  `HEALTH_CRITICAL` (1800–1802) transitions plus a per-session
  `HEALTH_SNAPSHOT` (1806) at most every 60 seconds. At `Level3`
  (`Debug`), the host additionally traces a debug-only line showing both
  host and client health facts together — this is a host-side observability
  trace and never changes the client's own logging profile.
- A shared, lock-free `Arc<AtomicU8>` aggregate (`service_health`,
  `fetch_max`-updated by every session, read-and-reset every 60 seconds by
  `serve()`'s existing TLS-health tick) drives one **service-level**
  `HEALTH_SNAPSHOT` reporting the worst state observed since the previous
  window, or `"unavailable"` — never a fabricated `"ok"` — if no session
  reported anything.
- `EFFECTIVE_PROFILE` (1805, always delivered regardless of active profile)
  reports the resolved `OperationalProfile` and its source once at startup
  and again after every successful `SIGHUP` reload.
- `netinfo.rs` probes `/sys/class/net` and `/proc/net/route` once per
  session (bounded reads) and emits one `NETWORK_PATH_ACTIVE` (1700) with the
  classified interface (`ethernet`/`wifi`/`vpn`/`loopback`/`other`), link
  speed (Mbps), and MTU. No raw SSID is disclosed (omitted by default,
  pending a future explicit policy). Per-tick `NETWORK_PATH_CHANGED`/`LOST`/
  `RESTORED` (1701–1703) monitoring is not yet wired into the live session
  loop — only the one-time session-start snapshot is emitted today (a
  documented, scoped gap; the `netinfo` builders for those transitions exist
  and are unit-tested but currently unused).

## Support bundles


`arcen-pier support-bundle [--out <DIR>]` dispatches before help parsing,
logging, TLS, or server startup. The default output is
`/var/lib/arcen/support`, mode `0700`; failure to create or write it is explicit
and requires `--out`.

The collector uses read-only log-root derivation and the existing recognized-log
inventory, preserving standalone `ARCEN_LOG_DIR` behavior without calling the
probing `logging::log_dir()`. It excludes `Xorg.log` and never traverses
`/run/arcen/sessions`, which contains Xauthority. Approved bounded diagnostics
include effective systemd configuration, service/runtime facts, command output,
and lifecycle excerpts whose journal fields or syslog hostname prefixes are
sanitized. Fixed 64 KiB streaming, 200 MiB included-log and 256 MiB total caps,
regular-file checks, same-directory partial publication, JSON redaction, and
typed source notices match the Windows adapter. Neither configured TLS
certificate/private-key path nor file is inspected, opened, hashed, or
recorded; bounded TLS lifecycle metadata can appear only through
already-sanitized logs/events. Neither archive names nor manifests contain the
hostname.

## Deferred / roadmap

- **Implement the dedicated-Xorg-per-session launcher** (lesson above) to replace the
  shared-`:0` path and the `--unsafe-allow-shared-display` flags.
- Multi-monitor (up to 4 heads) wiring end-to-end.
- Fine-grained NvFBC damage. Shared-CUDA ToCuda exposes only frame-level
  `bIsNewFrame`; public NvFBC 1.7/1.9 diff maps are ToSys/ToGL-only. A future
  producer must preserve zero-copy through an approved design or use an original
  CUDA comparison kernel.
- Complete and record the portable software physical-lab and distribution
  acceptance gates.

## Resume pointer

- **Status:** ✅ MIGRATED + DEPLOYED + LIVE-VERIFIED on pier-linux.example.internal
  (`203.0.113.10:18444/udp`, `arcen-pier.service`). The **dedicated-Xorg-per-session model**
  was migrated and deployed: `--session-gpu-head DFP-2 /
  :11`, `/etc/arcen/xorg.conf` template, `/run/arcen/sessions`, `--desktop-session gnome`.
  **User logged in as `jc` and streamed a real per-user GNOME desktop end-to-end.** Earlier
  shared-`:0` proof also passed (TLS 1.3, WS 101, `frames_sent=7`, NvFBC→NVENC). Remaining:
  multi-monitor (up to 4 heads) end-to-end. Linux Keel idle cadence and
  structured INFO telemetry are implemented offline; the live GRID
  idle/4K60/modeset checklist remains pending reviewer validation. The
  source-built software fallback is now implemented offline; physical
  dedicated-Xorg fallback, Deck, performance/allocation, and soak validation
  remains pending and is not inferred from the earlier NVENC evidence.
- **Original next step (done):** copy `hosts/linux/src` + `Cargo.toml` into `hosts/linux`, rename crate →
  `arcen-pier-linux`, lib `arcen_pier_linux`, bin `arcen-pier` (+ session-launcher/agent),
  fold audiocap/input_helper bins, imports → `arcen_protocol`; the historical
  migration port was 18443 and the current QUIC product port is 18444. Build on
  `pier-linux.example.internal` (`root@<your-pier-host>`): `cargo build --release -p arcen-pier-linux`. Deploy to
  `/opt/arcen/bin/` + `arcen-pier.service`.
