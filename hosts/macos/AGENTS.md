# macOS Pier Ownership

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

## What has been qualified against a real Deck

Measured from a Deck on another Mac against the installed Pier, not inferred
from code:

| Contract | Host served | Deck decoded |
| --- | --- | --- |
| Preset | Encoded (read from the SPS at both ends) | Deck decoded |
| --- | --- | --- |
| Auto / Speed | HEVC Main 4:2:0 8-bit, `420v` capture | `420v` → `Yuv420` |
| Grading | HEVC RExt 4:4:4 10-bit, BT.709, full range, `xf44` capture | `xf44` → `Yuv444` 10-bit |
| HDR (proven headroom) | HEVC RExt 4:4:4 10-bit, PQ/BT.2020/BT.2020, full range | `xf44`, Deck presents HDR10 (EDR) |

An earlier version of this table said Grading was served as `yuv444p10le`
from `x444`. That was the plan, not the stream: the bitstream was Main 4:2:0
8-bit, because no profile was set. Check the `encoded stream truth` and
`received stream truth` lines, not the hello.

Grading is precision and chroma resolution, not a wider gamut. HDR is a
different transfer, and is claimed only when the captured display reports
headroom above SDR white. See `STREAMING.md`, "Four pipelines".

Per-stage latency from one such session: capture-wait 6.94 ms, encode 9.02 ms,
send 0.02 ms, worst frame 32.83 ms, 49 frames.

**A full interactive session.** The Deck GUI, driven by hand rather than by a
smoke command: select the host, authenticate as the console owner, and the
remote desktop appears and keeps streaming — 3.5 minutes continuous, and a
25-second run repeated after every later change. Clipboard was proven live in
both directions during that session: a marker copied on the host arrived on the
client, and one copied on the client arrived on the host.

Interactive testing found two things four evenings of smoke commands had not.
Every smoke command authenticates as whoever owns the console, so none of them
could ever produce a console mismatch; the GUI, connecting as a different
account, timed out for ten minutes instead of saying so. And killing the GUI
mid-session showed `SESSION_END` reporting `frames_sent = 0` for a session that
had streamed for minutes. Run the GUI.

**Displays.** `single_primary` and `windowed` complete. Multi-monitor now has
the wire path as well as capture: `AuthMultiMonitorOfferMsg`, requested topology
admission, `ServerMultiMonitorMsg`, region video headers with nonzero monitor
id / topology generation / stream epoch, and one capture/encoder path per
admitted monitor. `platform.multi_monitor.advertise_enabled` still defaults
off, and the host also requires more than one attached display before it
advertises.

This is implemented, not qualified. The lab Mac used tonight has one display
attached, so the multi-display capture test skips and no real Deck has drawn a
two-display macOS Pier session. Do not call `match_layout` proven until a Mac
with two attached displays captures, negotiates, sends region frames, and the
Deck presents both.

Capture is one session per display, deliberately separate from the
single-display path. Most sessions are one screen, that is the path whose cost
matters, and making it carry a vector and a per-frame monitor identity to serve
the minority is how a fast path stops being fast.

**Input.** `input_applied` is non-zero with Accessibility refused, which
settles a recurring question: **pointer and scroll injection do not need
Accessibility; keyboard does.** The counter records what the host posted, not
what an application received, and those are different claims.

Keyboard is proven where the grant exists — `probe-keyboard` reports
`press_observed` and `release_observed` true, read back from HID system key
state rather than inferred from the injection returning. On a machine without
the grant the same probe fails, which is the correct and visible outcome.

Pen is proven end to end: a full stroke sent as a Deck would send it in light
mode produced 6 samples and 2 proximity edges with 0 rejected. The Deck's own
`input-smoke` cannot exercise this — it sends pen only in Hard USB mode, which
this host refuses — so Basic Tablet has its own test here instead.

**Audio negotiates and no longer blocks the desktop.** Measured end to end:
102 audio packets reached a client alongside 45 video frames. The current
end-to-end test `host_audio_reaches_the_client_when_capture_is_available`
asserts both sides of that claim: the host counted packets and the client
received audio frames.

Two things about it are not obvious and cost a day between them:

* Apple raises the **system audio recording consent prompt inside**
  `AudioDeviceCreateIOProcIDWithBlock`, and the call does not return until it
  is answered. On the lab Mac, telemetry reached `stage=ioproc` at 23:36:53 and
  `stage=device_start` at 23:37:53: `AudioDeviceStart` took sixty seconds.
  Capture therefore starts on its own thread under a 1500 ms budget and is
  collected only if it arrives in time. **Audio must never gate the desktop.**
* `supports_audio` is now advertised from the Screen Recording consent grant,
  because macOS gates system audio behind the same "Screen & System Audio
  Recording" approval. The host publishes `audio_output` capabilities in
  `server_hello`, resolves against the Deck's `audio_output`, sends
  `audio_stream_result` once it knows what this session will carry, and starts
  no tap for a session that did not negotiate audio. Without consent the host
  reports `CaptureUnavailable` and keeps streaming video without sound.

Do not infer audio from a tap that constructed: on a machine with capture
denied, a tap creates, reports a valid format, honours a mute request and then
delivers nothing. Read `audio_observed` from `probe-audio`.

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

`packaging/macos/build-pier-app.sh` used to *check* that
`target/release/arcen-pier-macos` existed and package whatever it found. A
telemetry fix was written, unit-tested, packaged, signed, installed on the lab
Mac and then measured still failing — because the binary inside the package
predated the fix by forty minutes. Every step in between was honest: the
signature verified, the installer succeeded, launchd restarted the agent on
the new file. The file was simply old.

The script now builds. When something you just fixed still misbehaves on the
lab, confirm the artefact before you re-read the code:

```sh
strings "Arcen Pier.app/Contents/MacOS/arcen-pier-macos" | grep -c <a-new-string>
```

A zero there means you are debugging a binary that does not contain your
change.

## Audio, end to end

Proven on the lab Mac after a clean uninstall and install, with sound playing:

```
media-smoke complete
audio_packets=6 audio_nonzero_samples=8903 audio_peak=1610
```

Non-zero samples and a non-zero peak: sound that was playing on the host
arriving at the client, not a channel that merely opened. A silent host still
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

Four things had to be true at once, and each hid the next: audio had to be
advertised (it deadlocked), the tap had to start without blocking the desktop
(`AudioDeviceStart` took sixty seconds waiting on a prompt), the tap had to
actually deliver (starting is not delivering — wait for the first callback),
and the capture had to reach the streamer. That last one was `serve_desktop`
being called with `None`, which alone would have kept every packet count at
zero however well the other three worked.

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
arcen-pier-macos new-host-cert      # certificate material, shared policy
```

`probe-media` takes `--frames`, `--codec hevc|h264`,
`--format bgra8|nv12|nv12-10|444-10` and `--dynamic-range sdr|hdr-local|hdr-canonical`.
Run capture probes one at a time: ScreenCaptureKit serves a single stream, and
concurrent runs fail with timeouts that look like permission faults.

`new-host-cert` takes the same flags as the Linux host — `--directory`,
`--dns`, `--renew`, `--new-key`, `--adopt-legacy` — because the decision it
makes comes from `arcen_transport::cert_provisioning` and must not differ by
platform. The adapter only reports what is on disk and carries out the plan.

Four things this has already caught, none of which a unit test would have:
keyframe detection that read every H.264 frame as a keyframe because the NAL
headers alias with HEVC; absolute pointer placement drifting because the HID
tap applies pointer acceleration; a permission report claiming input was
unavailable on a machine where it demonstrably worked; and keyboard injection
turning out to need Accessibility while pointer injection does not.

Requesting HDR is not proof of HDR. A ten-bit surface only shows bit depth. The
Pier creates an HDR virtual panel for an HDR request, and claims HDR only when
the display it captures reports potential headroom above 1.0. It then reads
the captured surfaces' tags back (`captured surface colour`). With
`ARCEN_HDR_PANEL=sdr` the same request degrades to Grading, and the Deck
reports it.

Native login, display, entitlement, signing, package, and release changes
require Release/Security review.

## Streaming evidence lives in STREAMING.md

What the pipeline measures at, and which theories about it turned out to be
wrong, are in [`STREAMING.md`](STREAMING.md). The short version, because each
of these was believed and is false:

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

## Packaging and install traps live in PACKAGING.md

Building a signed and notarized `.pkg`, and the install-time traps that
produced an afternoon of "it installed fine and the client times out", are
written up in [`PACKAGING.md`](PACKAGING.md). The short version, because each
of these cost real time:

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

## The lab Mac is a delivery target, not a workstation

Mac-S-08 exists to answer one question: does the shipped package install,
run and uninstall on a machine that never built it. That is only worth
anything while it stays a machine that never built it.

So nothing developer-side runs there. No `xcrun`, no `cargo`, no `clang`, no
toolchain install, no compiling a scratch probe in `/tmp`. Invoking `xcrun`
alone is enough to raise the Command Line Tools installer on the console,
which both interrupts whoever is sitting there and quietly turns the clean
machine into a dirty one.

Build and compile here, on the development Mac. Copy the finished artefact
across and run it. If something can only be measured by compiling on the
target, that is worth saying out loud rather than working around, because it
usually means the test is measuring the toolchain rather than the product.

## Virtual HID, and what it gates

`IOHIDUserDevice` is how a macOS host presents an input device it did not
receive from hardware. It is public SDK API, in IOKit's public module map, with
symbols in `IOKit.tbd`, available since macOS 10.15 and carrying no deprecation
in the current SDK. It needs no DriverKit extension, no system extension, no
user approval dialog and no reboot.

It needs `com.apple.developer.hid.virtual.device`, and that requirement was
measured rather than read. On Mac-S-08, macOS 26.5.2, a probe carrying no
entitlement:

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

`IOUSBHostControllerInterface` is the alternative and is the worse one. It needs
its own managed entitlement, and the reference material — a commercial product
that solved this same problem — does not use it: an exhaustive `nm -u` scan
across its 172 Mach-O files found no `IOUSBHost*` user, no `.dext`, no
`.systemextension` and no kext. Its session binary imports
`IOHIDUserDeviceCreateWithProperties` and carries
`com.apple.developer.hid.virtual.device` under a Developer ID signature, which
also settles the distribution question: this entitlement is obtainable outside
the Mac App Store.

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
   was measured on the lab. The first `CGEventCreateMouseEvent` the root agent
   made there never returned: a stack sample showed `SLEventCreate →
   CGSEventSourceForID → CGSEventSourceShutdown` waiting on a mutex no other
   thread held. It froze the whole session, not only input. The login window's
   backend is therefore `InputBackend::VirtualHidOnly`, whatever
   `input_backend` says. The `hid-injector` child owns a boot keyboard and an
   absolute pointer, both described in `arcen_input::hid_reports`. The absolute
   pointer measured exact, with no acceleration. The reference does the same
   split: its log says *"login window closed, switching to CG mouse input"*,
   and it creates a virtual mouse next to its keyboard.
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
and the login window has none. The reference's installer restarts its agent to
raise these prompts while somebody is still logged in, for the same reason.

**Apple publishes no contract that third-party code may capture the login
window.** Every piece above is documented on its own, but the combination is
observed practice. Cold-boot operation after a reboot, across macOS versions,
remains unqualified. It is a Release/Security decision before it is a supported
claim.
