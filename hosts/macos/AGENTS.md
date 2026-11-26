# macOS Pier Ownership

**Owner role:** macOS Pier (packaging co-owned with Release/Security)

Own only irreducible macOS host integration here: LoginWindow/Aqua session
activation, ScreenCaptureKit/VideoToolbox, CoreGraphics/AppKit input, native
pasteboard access, AudioServerPlugIn, USB host-controller integration, and
ServiceManagement/package adapters.

The Pier bundle's minimum macOS version is 14.2 because Core Audio process taps
are strongly imported by the audio lease path; do not lower packaging metadata
unless the tap symbols are weak-linked and availability-gated in code.

Keep negotiation, policy, validation, lifecycle, logging vocabulary, EDID
construction, media contracts, input state, clipboard limits, and USB bridge
state in `shared/`. Native adapters must fail closed until they provide
verified evidence. Never run the network or media stack as root.

Validate on macOS with:

```sh
cargo test --locked -p arcen-pier-macos
cargo build --locked --release -p arcen-pier-macos
```

## What each preset encodes

What the installed Pier sends, read from the SPS at both ends:

| Preset | Encoded (read from the SPS at both ends) | Deck decoded |
| --- | --- | --- |
| Auto / Speed | HEVC Main 4:2:0 8-bit, `420v` capture | `420v` → `Yuv420` |
| Grading | HEVC RExt 4:4:4 10-bit, BT.709 (transfer `srgb`, H.273 13, for a Deck that claims `srgb_transfer`; the SCK surface is sRGB either way), full range, `xf44` capture | `xf44` → `Yuv444` 10-bit |
| HDR (proven headroom) | HEVC RExt 4:4:4 10-bit, PQ/BT.2020/BT.2020, full range | `xf44`, Deck presents HDR10 (EDR) |

Verify the encoded SPS at both ends: read the `encoded stream truth` and
`received stream truth` lines, not the hello. A requested format is a plan,
not the stream: without an explicit profile, a session asked for 4:4:4
10-bit is encoded as Main 4:2:0 8-bit.

Grading is precision and chroma resolution, not a wider gamut. HDR is a
different transfer, and is claimed only when the captured display reports
headroom above SDR white. See `STREAMING.md`, "Four pipelines".

## Structured logging is how latency is proven

The Pier emits the same schema-validated lifecycle records the Linux host does,
through `arcen-observability` and `arcen-telemetry`, so one reader works on
either host. Records land in `/Library/Logs/Arcen/Pier/arcen-pier-macos.jsonl`,
or `~/Library/Logs/Arcen/Pier` when the agent cannot write the system path.
`ARCEN_LOG_DIR` overrides both.

`SERVICE_START`, `SESSION_AUTH_OK`, `SESSION_AUTH_FAIL`, `SESSION_STREAM_START`,
`SESSION_END` and `HEALTH_SNAPSHOT` are canonical. Per-stage timings and input
counters ride the diagnostic channel, because the canonical schema declares no
field for them and it is shared and append-only — extending it is a
Shared/Architecture change, not something to do in passing.

```
fps_actual, frames_captured, frames_encoded, frames_sent, bytes_sent,
mean_capture_wait_ms, mean_encode_ms, mean_send_ms, max_frame_ms,
input_applied, input_out_of_order,
pen_samples, pen_proximity_edges, pen_rejected
```

Report the stages separately. A slow encoder and a slow network both present as
a low frame rate, and only the split says which; capture-wait separates a host
that cannot get frames from one that cannot ship them.

Two fields exist because their absence was mistaken for success:
`SESSION_END.frames_sent` separates "a client connected" from "a desktop
arrived", and `audio_observed` separates a tap that was created from audio that
was heard. A session can authenticate, bind the right desktop, negotiate
1920x1080 HEVC, end cleanly, and have carried no picture at all.

## Test the binary you built, not the one that was lying around

`packaging/macos/build-pier-app.sh` builds the executable it packages; a
script that only checked that `target/release/arcen-pier-macos` existed would
package whatever it found. A signature that verifies, an installer that
succeeds and launchd restarting the agent on the new file say nothing about
whether that file contains the change.

When something you just fixed still misbehaves on a Pier, confirm the
artefact before you re-read the code:

```sh
strings "Arcen Pier.app/Contents/MacOS/arcen-pier-macos" | grep -c <a-new-string>
```

A zero there means you are debugging a binary that does not contain your
change.

## Audio, end to end

With sound playing on the host, `media-smoke` must report non-zero samples
and a non-zero peak, for example:

```
media-smoke complete
audio_packets=6 audio_nonzero_samples=8903 audio_peak=1610
```

That is sound that was playing on the host arriving at the client, not a
channel that merely opened. A silent host still
completes in about four seconds, because a Core Audio tap delivers silence
buffers continuously rather than going quiet.

Two permissions gate this, and they are **different lists**:

| Pane | Service | Gates |
| --- | --- | --- |
| Screen & System Audio Recording | `kTCCServiceScreenCapture` | ScreenCaptureKit — video |
| System Audio Recording | `kTCCServiceAudioCaptureOnly` | Core Audio process taps — host sound |

A host holding only the first streams video and captures silence. No public
call reports the second, so the session attempts the tap and degrades rather
than preflighting.

Four conditions must hold together, and each can hide the next: audio is
advertised; the tap starts without blocking the desktop (`AudioDeviceStart`
can wait sixty seconds on a consent prompt); the tap delivers (starting is
not delivering: wait for the first callback); and the capture reaches the
streamer (`serve_desktop` gets the capture, never `None`). Check all four
before you trust a packet count.

## Nothing can grant a permission that was never asked for

macOS lists a subject under Privacy & Security only once it has asked, and for
system audio the only thing that asks is creating a Core Audio process tap.
There is no preflight call. A host that never creates one produces no prompt
and no entry in the pane, so the grant cannot be given — which reads exactly
like a refused permission and is not one.

```sh
arcen-pier-macos register-audio-consent   # creates a tap, drops it, lists the app
```

Run it once, from the account that will serve, then switch the app on under
**System Audio Recording**. That is not the same list as **Screen & System
Audio Recording**: the first is `kTCCServiceAudioCaptureOnly` and gates Core
Audio process taps, the second is `kTCCServiceScreenCapture` and gates
ScreenCaptureKit. A host that holds only the screen grant streams video and
captures silence, and no public call reports the audio one — which is why the
session attempts the tap and degrades rather than preflighting.

Listing is not granting; a session will keep reporting `CaptureUnavailable`
until someone approves it.

It is deliberately not part of `serve`. Creating a tap holds the output device
briefly, and doing that on every launch contends with the first session's own
tap.

## Probes cannot be run over SSH

TCC attributes a request to the process that asks. Over SSH that is `sshd`,
not the bundle, so `probe-audio` run from a remote shell reports a tap that
creates, formats correctly, honours mute — and delivers zero callbacks,
exactly as if consent had been refused. That result says nothing about the
machine. Anything that depends on a privacy grant has to run from the Aqua
session, or be measured through a real session served by the installed agent.

## Proving native behaviour

Unit tests cannot prove that a system framework does what its documentation
says. Every native capability has a probe that runs on real hardware and
reports what the machine produced, and a probe must exit non-zero when it
proves nothing usable:

```sh
arcen-pier-macos inventory          # displays WindowServer reports
arcen-pier-macos permissions        # capture / pointer / keyboard capability
arcen-pier-macos probe-media        # ScreenCaptureKit -> VideoToolbox
arcen-pier-macos probe-input        # pointer placement, read back and restored
arcen-pier-macos probe-keyboard     # key delivery via HID system key state
arcen-pier-macos probe-clipboard    # pasteboard round-trip and change tracking
arcen-pier-macos probe-audio        # tap, format, mute evidence, samples heard
arcen-pier-macos probe-virtual-hid  # may this signature create a HID device
arcen-pier-macos probe-pen          # lab builds only: Tablet support pen, read back from AppKit (hovers unless --strokes)
arcen-pier-macos probe-pen-hid      # what a virtual pen digitizer delivers (hovers only)
arcen-pier-macos probe-bridged-hid  # lab builds only (lab-bridged-hid-probe): bridged tablet devices as the HID system sees them
arcen-pier-macos new-host-cert      # certificate material, shared policy
```

`probe-media` takes `--frames`, `--codec hevc|h264`,
`--format bgra8|nv12|nv12-10|444-10` and `--dynamic-range sdr|hdr-local|hdr-canonical`.
Run capture probes one at a time: ScreenCaptureKit serves a single stream, and
concurrent runs fail with timeouts that look like permission faults.

`new-host-cert` takes the same flags as the Linux host — `--directory`,
`--dns`, `--renew`, `--new-key`, `--adopt-legacy` — because the decision it
makes comes from `arcen_transport::cert_provisioning` and must not differ by
platform. The adapter only reports what is on disk and carries out the plan:
what to issue (profile, kept names, kept SEC1 or PKCS #8 key) is
`arcen_transport::cert_issuance`, and the publish order and its journal are
`cert_transaction::PublicationPlan`; never add certificate parameters or a
transaction order here. A running service or `serve` reloads its material on
`SIGHUP` (`tls_material::spawn_watch`) through the shared
`HostTlsMaterial::reload`, and emits the shared certificate events.

Probes catch what unit tests cannot: H.264 and HEVC NAL headers alias, the
HID tap applies pointer acceleration to absolute placement, and keyboard
injection needs Accessibility while pointer injection does not.

Requesting HDR is not proof of HDR. A ten-bit surface only shows bit depth. The
Pier creates an HDR virtual panel for an HDR request, and claims HDR only when
the display it captures reports potential headroom above 1.0. It then reads
the captured surfaces' tags back (`captured surface colour`). With
`ARCEN_HDR_PANEL=sdr` the same request degrades to Grading, and the Deck
reports it.

Native login, display, entitlement, signing, package, and release changes
require Release/Security review.

## Streaming evidence lives in STREAMING.md

What the pipeline measures at, and what the measurements rule out, are in
[`STREAMING.md`](STREAMING.md). The short version, because each of these is
a plausible belief that is false:

- VideoToolbox **already uses hardware**, measured by read-back at every size
  and both codecs. Not requesting hardware proves nothing; hardware has been
  the default since macOS 10.15.
- A low frame rate with `dropped_frames=0` is **not** evidence that nothing
  was lost. Starving the ScreenCaptureKit pool stops delivery upstream of that
  counter, measured.
- The local suite **cannot** reproduce a slow writer: loopback sending costs
  0.05 ms per frame against 25.84 ms recorded on a real link.
- A development machine has **no idle desktop**. The terminal running the
  tests is what is animating the screen.

## Keep install targets clean

Installer qualification asks whether the shipped package installs, runs and
uninstalls on a Mac that never built it. Run no developer tool there: no
`xcrun`, `cargo`, `clang` or `python3`, no toolchain install, no scratch
probe compiled in `/tmp`. Each can raise the Command Line Tools installer on
the console and leaves the Mac no longer clean. Build and compile on a
development Mac, copy the finished artefact across, and use `grep`, `sed`
and `awk` on the target. If something can only be measured by compiling on
the target, say so.

## Packaging and install traps live in PACKAGING.md

Building a signed and notarized `.pkg`, and the install-time traps that
make "it installed fine and the client times out", are written up in
[`PACKAGING.md`](PACKAGING.md). The short version:

- Signing the installer needs a **different certificate** from signing the
  apps inside it.
- `launchctl disable` is permanent. It outlives the uninstall and makes every
  later install fail with `Bootstrap failed: 5`. Use `bootout`.
- Never `2>/dev/null || true` the command that starts the product.
- The installer must write `pier.json`; without it the host falls back to
  loopback-only and is unreachable.
- Verify an uninstall by **path**, and finish with a `find` sweep —
  `/etc/pam.d/arcen` is the one every hand-written list misses.
- Two processes: `daemon` (LaunchDaemon, `_arcen`, UDP 18444, key, admission)
  and `agent` (LaunchAgent in every Aqua session, desktop only), joined by
  `arcen_session::agent_relay` over a local socket. Never give a file to the
  person who ran the installer; `serve` remains the single-process mode for
  tests and hand runs.
- The network side runs every session on isolated QUIC lanes (ADR 0014,
  protocol v5). `relay::relay_lanes` is the only code that touches them, for
  the daemon and for `serve` alike: it grants the attachment after a
  successful `auth_result` (or before `server_hello`) and hands everything
  else to the shared `arcen_transport::quic::PierLaneRelay`. The session code
  still speaks one local WebSocket; the agent's audio goes to the daemon on
  its own side channel and from there onto the audio lane. Do not route media
  onto the control lane or reintroduce a single Deck stream.

## Virtual HID, and what it gates

`IOHIDUserDevice` is how a macOS host presents an input device it did not
receive from hardware. It is public SDK API, in IOKit's public module map, with
symbols in `IOKit.tbd`, available since macOS 10.15 and carrying no deprecation
in the current SDK. It needs no DriverKit extension, no system extension, no
user approval dialog and no reboot.

It needs `com.apple.developer.hid.virtual.device`. Without it, creation fails
silently (macOS 26.5.2, a probe carrying no entitlement):

| Run | `IOHIDUserDeviceCreateWithProperties` | `errno` |
| --- | --- | --- |
| ordinary user | `NULL` | 0 |
| under `sudo` | `NULL` | 0 |

Three things follow, and each changes a plan someone would otherwise make.

**Root is not a substitute.** The obvious workaround does not work. The
entitlement is now granted for `pier.arcen.tech`, so only the Pier's signature
can create devices. The app *responsible* for the creating process must also
hold Accessibility, which is why the Agent Helper starts the Pier binary as its
`hid-injector` child.

**AMFI does not kill the process.** Creation simply returns `NULL`. That is the
useful half: the importer can be written, signed, shipped and exercised now, and
it will fail closed with a precise reason until the entitlement arrives, rather
than terminating before `main` the way an unauthorized entitlement request does.
A host that refuses Native Tablet with "virtual HID unavailable" is correct
behaviour, not a stub.

**Arcen's three DriverKit HID entitlements do not apply.** `driverkit.transport.hid`
is permission to interact with *hardware*; Arcen's pen samples arrive over a
network. They are the wrong axis entirely.

`IOUSBHostControllerInterface` is the alternative and is the worse one: it
needs its own managed entitlement, a user-mode host controller, and none of
it is required to present a HID device. The virtual HID entitlement, by
contrast, is granted to Developer ID applications, so it is obtainable
outside the Mac App Store (the Pier's own profile carries it).

## A virtual pen digitizer, measured

The obvious design for a pen on a macOS host is a virtual HID digitizer: a
generic pen (`arcen_input::hid_digitizer::PEN_DESCRIPTOR`, Digitizers page,
tip/barrel/invert/eraser/in-range, 16-bit X/Y, 8192 pressure levels, tilt and
twist) published through `IOHIDUserDevice` with the entitlement, so macOS's own
HID stack would produce tablet events for every application. With the
tablet maker's driver installed on the host, that driver is what turns the
reports into pressure.

Without a vendor driver it does not work, and that is measured, not read. On
macOS 26.7.2, entitled, with an AppKit window and a listen-only event tap
reading back what the window server posted:

| | Generic HID digitizer | CoreGraphics tablet events (what the Pier posts) |
| --- | --- | --- |
| Tablet proximity event, pen/eraser type | yes, capability mask `0x2587` | yes, mask `0x25c7` |
| Pointer events with the tablet subtype | yes | yes |
| `NSEvent` pressure / tilt / rotation | **0 / 0 / 0, always** | the values sent |
| Separate tablet-point events | posted, tilt mis-scaled (raw / 32767), rotation raw / 64, pressure 0; **an AppKit window received none** | not needed |
| When the tip goes down | at 75 % pressure (`kIOHIDDigitizerTipThresholdKey`), not the Tip Switch | at the Deck's touch edge |
| Where it lands | the display under the pointer, whichever that is | the exact point asked for |

So Tablet support on a macOS Pier stays on CoreGraphics. Nothing about it needs
the entitlement or root; pointer injection does not even need Accessibility.
`arcen-pier-macos probe-pen-hid` re-asks the question on whatever macOS it runs
on — it creates the digitizer, puts the pointer on the main display, hovers the
pen with tilt and rotation, never presses, and prints what arrived — so the
decision can be revisited when Apple's answer changes. It needs the same
signature and Accessibility as the injector, plus permission to observe events.

Two facts from that work are worth having before anyone touches the login
window's pointer: macOS maps an absolute HID pointer or digitizer onto **the
display the pointer is on** at each report (warp first, then report), and a
device created with tip pressure decides contact from the pressure threshold
whatever its Tip Switch says.

## Native tablet: one path

One Native tablet way on this host: the Pier reads the pen itself, and any
tablet works by default (ADR 0023 amendment 2026-11-15). A known model
keeps its dedicated reader (`arcen_usb_bridge::IntuosPenReader`, with the
Wacom mode switch); any other tablet is read by its pen interface's own
report descriptor (`arcen_usb_bridge::generic_pen`, chosen by
`host_pen_route` in `bridged_tablet::authorize`). Only a pen in a vendor
format no reader knows is refused, with its ids. A pen interface that also
declares a keyboard or mouse is read by the Pier alone
(`HidHostAdapter::with_host_read_interface`) and never published; with
nothing publishable no `hid-injector` child starts.
There is no proximity-only fallback and no vendor-driver detection. Where the
pen points is the shared path of
[ADR 0023](../../docs/adr/0023-one-native-tablet-pen-position-path.md):
`bridged_tablet::host_pen_sample` is the adapter (device units →
`arcen_input::tablet_pointer::PenSample`), `HostPenPointer` is glue over the
shared `PenPointerLead` and `HostPointerFeed`. Never map pen fractions to
pixels here; the injected event and the reported position use the same
scaling. The Deck draws the same pen dot as for a Linux Pier. Proofs: unit
tests here, and a real tablet in Chrome (Safari reports every pen as a
mouse).

## Native tablet: the Pier as USB host driver

[ADR 0022](../../docs/adr/0022-macos-pier-native-tablet-hid-adapter.md)
(Proposed). With the default feature `native-tablet` the session runs the shared USB
host driver (`arcen_usb_bridge::HidHostAdapter`) over the Deck's bridged
tablet and the existing `hid-injector` child, started as
`hid-injector --bridged-usb`, presents one `IOHIDUserDevice` per HID
interface. `bridged_tablet.rs` is the session side, `input/bridged_hid.rs`
the child. Release Piers carry it and offer it when signed
`--with-virtual-hid` (the entitlement is the gate). Every tablet's pen is
read by the Pier and posted as CoreGraphics tablet events with pressure: a
known model (Intuos5 touch L) by its dedicated reader (ADR 0022 §14.1), any
other by the generic reader (ADR 0023 amendment 2026-11-15).
`probe-bridged-hid` writes the Wacom mode switch, so it is built
only with `--features lab-bridged-hid-probe`.

- **First connection.** Devices hot-plug into the running Aqua session, the
  Deck names its tablet in the same connection's client hello, and the hello
  waits (≤ 3 s) for a capability probe still running instead of refusing.
  Keep all three: a hello that says "still checking" is a lost first
  connection.
- **Creation needs the responsible app's Accessibility**: the
  same signed binary creates devices when started from a terminal that holds
  it, and is refused as its own launchd job on a Mac where the app has no
  grant. In the product the responsible app is the Agent Helper.
- **Published devices are system-wide.** There is no seat to confine an
  `IOHIDUserDevice` to, so only pen digitizer and vendor interfaces are
  published (`bridged_tablet::PUBLISH_POLICY`, shared `publishable`), never a
  keyboard, keypad or mouse, nor anything on Apple's own vendor pages
  (`APPLE_EVENT_PAGES`: vendor keyboard `0xFF01` is Spotlight, Launchpad, Fn
  and brightness), and `SessionTablet` kills the device owner the
  moment the console leaves this agent's account (`ConsoleWatch`, re-read
  every 100 ms and before forwarding USB traffic). Keep both: without them a
  Fast User Switch hands the next account a live keyboard.
- **macOS rewrites `Transport` to `"Virtual"`** for an unprivileged creator,
  and the tablet maker's driver (Wacom Tablet 6.4.9) does not open the
  bridged devices, so pressure comes only from the Pier's own reader below.
- **Pressure: the Pier reads a known pen itself (ADR 0022 §14).** Part of Native tablet, no extra feature. For a
  model the shared reader knows (`arcen_usb_bridge::IntuosPenReader`,
  `HOST_DECODED_INTUOS_MODELS`, today the Intuos5 touch L only) the session
  reads the pen's Wacom mode (`GET_REPORT(Feature 2)`), switches it
  (`[2,2]`), reads the pen interface's packets instead of handing them to
  the published device, and posts CoreGraphics tablet events through
  `InputSession::apply_host_pen` — the route Wacom's own driver uses (its
  `com.wacom.IOManager` imports `CGEventPost`). When the attachment ends with
  the Deck still connected it puts back the mode it read (the request is
  left to finish, not cancelled); the Deck's hand-back reset restores the
  power-on mode in any case. Any other model keeps the published HID device
  and the no-pressure notices. The Deck is told `HOST_PEN_NOTICE`.
- **The cursor follows the pen on the Deck.** A bridged tablet moves only
  this host's pointer, so for a Deck that declared `cursor_position_v1` with
  a local cursor the single-display stream reports where each posted pen
  sample put the pointer (`bridged_tablet::HostPenPointer`, shared
  `HostPointerReporter`: first rest position never sent, ≤ 120 Hz, newest
  wins) as `CursorPosition` records; the Deck draws its cursor there until
  its own mouse moves. `cursor_mode_result.cursor_positions` says so.
  The daemon relays them through the shared `CursorPositionRelay`
  (`PierLaneRelay`). Multi-display sessions report none yet (ADR 0022
  §14.1).
- The ignored end-to-end tests drive the emulated Intuos5: with the host pen
  reader they **post pen events with pressure on the Mac that runs them**.
  Never run them on a Mac someone is using; `probe-pen --intuos5` presses
  too — run it only with a window you own proven topmost under the middle
  half of the main display.
- Prove it with the ignored end-to-end test
  `a_bridged_tablet_is_published_as_hid_devices_fed_and_torn_down`
  (`ARCEN_E2E_PIER_PROGRAM=<signed Pier binary>`, run from a terminal
  with Accessibility; `ARCEN_E2E_TABLET_HOLD=1` holds the devices for 60 s;
  `ARCEN_E2E_TABLET_CONSOLE=login-window|other-user` then hands the console
  away in-process and asserts the devices go)
  and look from outside with `ioreg -r -c IOHIDUserDevice -l`, `hidutil
  list` and `arcen-pier-macos probe-bridged-hid [--mode-switch] [--count S]`
  (a Pier built with `--features lab-bridged-hid-probe`).
  The test hovers the emulated pen and never clicks.

## The login window: what is measured, and what is not claimed

The login window is a different GUI session, not a logged-in one. Reaching it
takes four things together:

1. **The agent in both sessions.** A LaunchAgent with
   `LimitLoadToSessionType = [Aqua, LoginWindow]`; Apple's own
   `com.apple.screensharing.agent` does the same. launchd starts the
   LoginWindow instance as root. Its first window-server connection can fail
   when it starts in the same second as `loginwindow`, and that failure sticks
   for the life of the process. So the agent exits with 75 and launchd starts a
   fresh one.
2. **The privileged daemon** it attaches to over the agent socket
   (`agent_relay`).
3. **Virtual HID for all input: keyboard and pointer, never `CGEvent`.** This
   was measured. The first `CGEventCreateMouseEvent` the root agent
   made there never returned: a stack sample showed `SLEventCreate →
   CGSEventSourceForID → CGSEventSourceShutdown` waiting on a mutex no other
   thread held. It froze the whole session, not only input. The login window's
   backend is therefore `InputBackend::VirtualHidOnly`, whatever
   `input_backend` says. The `hid-injector` child owns a boot keyboard and an
   absolute pointer, both described in `arcen_input::hid_reports`. The absolute
   pointer measured exact, with no acceleration. A signed-in desktop's
   agent uses its configured `input_backend` again.
4. **An authorization plug-in**, in `/Library/Security/SecurityAgentPlugins`,
   for single sign-on into the login exchange. This one is *not* built.
   Signing in means typing the password into the streamed login window, which
   works.

**Signing in is a hand-over, not a disconnect.** The login-window session ends
when someone signs in, so its hello sets `login_window: true`
(`AdvertisedCapabilities::login_window`, taken from `SessionPolicy`). A Deck
that sees that session close holds its last frame and reconnects. The daemon
then attaches it to the signed-in user's agent, which registers within about
two seconds. Keep the flag truthful: a signed-in desktop that claimed it would
make the Deck keep credentials it should have dropped.

**Consent is not headless.** Screen capture at the login window raises a
consent prompt in the root context. Until someone at the machine answers it,
ScreenCaptureKit delivers a few frames and then stalls. TCC consent is per user,
and the login window has none. Consent prompts must therefore be raised and
answered while somebody is logged in.

## Joining the user's session (ADR 0025)

* **Disconnect locks.** When a signed-in user's Deck session ends, the agent
  locks the session (`screen_lock`: the system's Lock Screen,
  `SACLockScreenImmediate` found with `dlsym`, proved from
  `CGSessionCopyCurrentDictionary`'s `CGSSessionScreenIsLocked`), then drops
  the virtual displays, then releases the sound (`DETACH_STEPS`). Not the
  login window, not `serve` (it would lock the developer's own screen), and
  not a console another account took over (`Departure::SessionChanged`).
  There is no public unlock after a password check: a returning Deck signs
  in at the streamed lock screen. A lock that is refused is asked again
  every 5 s for at most `UNLOCKED_GRACE`, desk privacy or not (shared
  `console_session::lock_wait`), and an ending that leaves the desk
  unlocked is logged as an error. Under `privacy.desk = "required"` a
  private desk is never given back unlocked (`WaitingDesk::Required`):
  past the grace it stays dark with its input blocked, the refusal is
  logged once as an error (later refusals are debug lines), the slot is marked (`set_lock_refused`) so a
  Deck that signs in is refused `HostLockRefused` (worded as busy), and
  the lock is asked for every `HELD_LOCK_RETRY` (30 s) until it comes or
  the console leaves the account. A private desk holds the session slot
  for that wait; a desk that was not private shows the session
  anyway, so it offers the slot (`SessionAdmissionRuntime::offer_yield`)
  and an authenticated Deck that finds it taken asks for it
  (`request_yield`), waits up to `YIELD_WAIT`, and is served, ending the
  wait. Whether the console is still this account's is read fail-closed
  (`console::console_is_ours`, shared `console_still_ours`: the console
  user, or this session's own `kCGSessionOnConsoleKey`), so a failed read
  never skips the lock.
* **Stop Sharing ends the attachment, never the session.** Since macOS 14
  every `ScreenCaptureKit` stream shows the system's "Currently Sharing"
  menu-bar item with **Stop Sharing**, which anyone at the desk or on the
  stream can press. `stream:didStopWithError:` keeps the error's domain
  and code (`StreamStop`), and only `SCStreamErrorDomain` -3817 (a person
  pressed Stop Sharing; shared `stream_end::capture_stop_cause`) ends the
  stream as `CaptureStopped`: the Deck gets the close reason
  `DECK_REASON_SHARING_STOPPED` and does not reconnect, `SESSION_END` says
  `capture_stopped`, and the ordinary hand-back locks for
  `Departure::CaptureStopped` and releases the guard (`end_attachment`).
  Never restart the capture: someone stopped sharing on purpose. Every
  other stop (the system's own, -3821; a lost window-server connection,
  -3805; a display gone) is a capture failure: no close reason,
  the Deck may reconnect, the session is locked.
* **An agent that dies locks too** (`lock_guard`, shared
  `console_session::owner_death`). For each Deck attached
  to a signed-in session, the agent starts `lock-guard`: the same binary, in
  its own process group. launchd sends `SIGTERM` to a dead agent's process
  group and only that group; this was measured with a `KeepAlive` agent
  killed by `SIGKILL`. If the guard's pipe closes without a release, it
  darkens the desk (when the desk was private), locks, proves the lock and
  gives the displays back. The agent also writes
  `~/Library/Application Support/Arcen/Pier/deck-attachment`, naming the
  security session (public `SessionGetInfo(callerSecuritySession)`: the
  audit session every process of the login session shares, and the `asid`
  of launchd's `gui/<uid>` domain a restarted agent starts in),
  `kern.bootsessionuuid` and the uid. A restarted agent that finds its own
  session's record locks before it serves anything (shared
  `startup_recovery_in`). It fails closed: only a missing record is no
  record (any other read error locks), the record is removed only once the
  session is locked, and a record of another login session of this boot
  and account (an agent started over SSH) is left alone; only a new boot or
  account makes it stale. Never read the session from
  `CGSessionCopyCurrentDictionary`'s `kCGSSessionIDKey`: that dictionary
  does not carry it (macOS 26), and no record would be written. Between the death and the lock screen the desk's input
  is not filtered, because the tap's Accessibility grant is the agent's.
* **Awake while serving.** Every connection declares user activity first (a
  sleeping display is not an active one, and the inventory would be empty),
  and a session holds a `PreventUserIdleSystemSleep` assertion (`power`).
* **One sign-in throttle per Mac.** Each agent is its own process, so the
  network service applies the shared throttle to the `auth_response` it
  relays and learns the outcome from the agent's `auth_result`
  (`relay::SignInGate`). The agent keeps its own for `serve`.
* **Desk privacy** (`desk_privacy.rs`, opt-in
  `platform.desktop.deskside.enabled`, as Windows): armed after the session
  is admitted and before anything is shown, for a signed-in user's agent
  only. Input: an active `CGEventTap` at `kCGHIDEventTap` passes only events
  the window server attributes to this process
  (`kCGEventSourceUnixProcessID`; hardware and virtual HID read `0`), so it
  needs the Accessibility grant keyboard input has and refuses
  `input_backend: virtual_hid`. Displays: an all-zero
  `CGSetDisplayTransferByTable` on every display but the Pier's own virtual
  ones (vendor `0x3456`), read back, re-asserted every 100 ms; a display
  lit for 2 s ends the session (shared `PrivacyWatch`). Hand-back: lock
  (retried every 5 s, at most `UNLOCKED_GRACE`; under `required`, until
  it locks), displays, sound, input; a
  console that leaves the account for three readings gets the desk back and
  ends the session with what the shared `WatchVerdict::deck_refusal` says
  (read fail-closed, `console::console_is_ours`, as the lock reads it):
  `console_other_user` (naming the account) when another account holds it,
  `console_session_changed` when the login window does, never
  `console_privacy_unavailable`, which is only for a display that stayed
  lit; a panic locks first. Both die with the process; the
  lock guard below locks the session after them. Not covered: Secure Event Input hides keystrokes
  from taps; Siri, Voice Control and dictation act through accessibility;
  DisplayLink/Sidecar/AirPlay displays are fed by capture. `probe-desk-privacy [--listen S] [--blank S
  --capture]` measures it from the Aqua session: hardware events read pid
  0, the probe's own posts read its pid, and `--capture` shows whether
  `ScreenCaptureKit` reads before the table.

**Apple publishes no contract that third-party code may capture the login
window.** Every piece above is documented on its own, but the combination is
observed practice. Cold-boot operation after a reboot, across macOS versions,
remains unqualified. It is a Release/Security decision before it is a supported
claim.
