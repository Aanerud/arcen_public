# clients/macos — `arcen-deck-macos` (Arcen Deck)

**Delivery:** Arcen Deck, the macOS client. Product surface. Binary `arcen-deck`, app
`Arcen Deck.app`, bundle id `com.example.arcen.deck`.

## What this is

The thin client optimized for macOS: an egui/eframe UI (connection home, session view,
settings), QUIC/TLS 1.3 transport to an Arcen Pier, a decode pipeline (VideoToolbox H.265/H.264
→ frame queue → render), audio playback (cpal), input capture forwarded to the Pier, and
HID device passthrough (HoIP) — physical Wacom/tablet USB reports forwarded verbatim over
the session wire so the remote host sees a virtual pen tablet with full pressure/tilt.
It is deliberately separate from any future Windows/Linux Deck so each stays optimized for
its platform's decode/render path.

## How it's grounded

- Lessons encoded:
  - **Default profile = HEVC 4:4:4 @ 60fps** (not h264/4:2:0/15) — creative-pro fidelity is
    the point; CLI flags still override (`initial_stream_profile()` in `ui/app.rs`).
  - **The framed handshake is tolerant.** `transport/websocket.rs::recv_json`
    loops over `Close`/`Ping`/`Pong` control frames on the QUIC stream instead of
    erroring "expected text, got control frame", and surfaces the host's close
    reason as `ClosedByHost(String)`.
  - **One rustls.** rustls 0.23 is shared by tokio-tungstenite 0.24 and Quinn;
    native cert roots come from `rustls-native-certs`. Keep a single TLS stack.
  - macOS decode/UI uses `objc2`/`videotoolbox`/`core-graphics`; display physical-size +
    identity is captured for host EDID synthesis.

## Rules — what it must be (invariants)

1. Depends only on the active shared client surfaces (`arcen-input`, `arcen-media`,
   `arcen-observability`, `arcen-protocol`, `arcen-telemetry`, and
   `arcen-transport`) plus approved third-party crates.
2. Never depends on a Pier crate or the gateway. The only contract to the host
   is the wire protocol over QUIC.
3. Platform code (`objc2`, VideoToolbox, AppKit) stays behind `cfg(target_os = "macos")`;
   the crate must still `cargo check` on non-macos for protocol/transport unit tests.
4. `unsafe` is allowed (FFI to Apple frameworks, ~20 sites) but scoped and justified.
5. User-facing strings, window/app/menu titles, bundle id use **Arcen Deck** / `com.example.arcen.deck`.

## Interfaces / boundaries

- **Consumes:** `arcen-input`, `arcen-media`, `arcen-protocol`; a running Arcen
  Pier over QUIC on the configured UDP port (18444 by default).
- **Exposes:** the `Arcen Deck.app` GUI (Performance Mode + Colour Fidelity)
  plus the `arcen-deck` diagnostic CLI. Exact codec/chroma/variant flags are
  engineering-only, not production user choices.

## Module map (from the proven source)

- `main.rs` / `lib.rs` — entry + crate root.
- `credentials.rs` — credential entry/storage (zeroized).
- `clipboard.rs` — capacity-one inbound/outbound slots, host-policy mapping,
  generation-scoped UI controller, and the main-thread AppKit pasteboard adapter.
- `reconnect.rs` — the single memory-only resume credential owner and deterministic
  backoff/deadline state machine. It binds grants to endpoint, TLS/security, topology, and
  connection generation; manual/configuration/terminal paths zeroize and cancel it.
- `ui/` — `app.rs` (main app + `initial_stream_profile`), `home.rs`, `session`, `settings`,
  `theme.rs`, `keyboard.rs`, `media_worker.rs`, `macos_menu.rs`.
- `transport/` — `websocket.rs` (QUIC socket plus tolerant framed `recv_json`; the filename reflects the internal tungstenite message-framing codec, not a network WSS connection),
  `connector.rs`, `tls.rs`, `mod.rs`.
- `pipeline/` — `video_decoder.rs` (VideoToolbox), `frame_queue.rs`, `audio.rs`.
- `protocol/` — `keymap.rs` (client-side key mapping), `mod.rs`.
- `tablet/` — the default, non-experimental typed-pen capture path: `monitor.rs`
  (main-thread AppKit `NSEvent` tabletPoint/tabletProximity local-monitor RAII
  guard), `mapper.rs` (pure native-sample → `arcen_input::PenEvent` mapper),
  `dispatch.rs` (edge-preserving/motion-coalescing bounded dispatch), `probe.rs`
  (USB-presence + empirical axis-capability probe), `sample.rs`, `runtime.rs`
  (`TabletRuntime` RAII), `mod.rs`. See
  [`../../docs/architecture/pen-tablet-input.md`](../../docs/architecture/pen-tablet-input.md)
  and "Tablet / pen input" below.
- `hid/` — `iokit.rs` (raw IOKit FFI: IOHIDManager, IOHIDDevice), `session.rs`
  (`HidSession` + `HidEvent` — runs IOHIDManager on a dedicated CFRunLoop thread, filters
  to tablet vendors, sends DeviceAdded/DeviceRemoved/Report events upstream), `mod.rs`.
  Filtered vendors: Wacom 0x056A, Huion 0x256c, XP-Pen 0x28bd, UC-Logic 0x5543, Gaomon 0x0b57.
  **Quarantined:** `HidSession` is not started in ordinary sessions. It compiles only
  behind the default-off `experimental-raw-hid` Cargo feature and additionally
  requires a runtime opt-in (`ARCEN_EXPERIMENTAL_RAW_HID=1`) plus a mutually
  negotiated `experimental_raw_hid` wire capability with the host before it will
  start — see
  [`../../docs/architecture/pen-tablet-input.md`](../../docs/architecture/pen-tablet-input.md)
  ("Experimental raw-HID passthrough"). It is unrelated to, and not required by,
  the `tablet/` typed-pen path above.
- `display/`, `logging/` — display identity capture, tracing (`arcen::<area>`).
- Each connection attempt generates one UUID-v4 `CorrelationId`, sends it in
  `AuthResponse.session_log_id` (and the late `ClientHello` consistency echo),
  and records it as the root tracing field `sid`.
- Auto reconnect is explicit opt-in. Resume attempts never retain or resend the password,
  accept only transient transport failures, rotate the host grant before media continues,
  and start a fresh inbox/decoder/audio worker gated on a requested keyframe.

## Saved connections and settings

`connections.json` is a versioned saved-connection document. Each `SavedConnection` entity has a
stable ID, canonical endpoint identity (kind, normalized host, port, transport, and security-mode
discriminator), and owns a versioned `ConnectionSettings` value object. Only the optional
remembered username is connection-scoped today; passwords and resume credentials are never
serialized. Selecting, editing, or quick-connecting resolves the exact connection, so usernames
cannot cross hosts, ports, transports, or distinct security modes.

A validated nonempty username is saved to that exact connection once Deck queues the credential
submission, before it waits for the host's authentication/session result. Consequently a later
console, display, or session-launch failure cannot roll the submitted username back. Empty,
control-character, and over-255-byte values never replace a saved username; passwords remain
memory-only. If a host presents a disclaimer, Deck discards deferred credentials, then restores
the username from the exact saved connection only after the user accepts; no deferred password
or username crosses the disclaimer boundary.

Legacy global usernames migrate only to an identifiable last-selected connection. An
unidentifiable global username is discarded; no global fallback remains or is copied to a host.
Unknown document, connection, and per-connection settings fields survive read/write cycles.
Cursor, display, clipboard, and performance choices remain global; the nested settings object
is the planned migration point if those options become connection-specific later. See
[`SETTINGS.md`](SETTINGS.md) for schema and migration details.

Persistence is injected through a config repository. Production resolves the platform config
root once; tests use explicit repository-local paths, while the test-default repository has no
root and cannot resolve or write a production config even during parallel execution or panic.
Saved-connection commands take an advisory exclusive lock on the adjacent lock file, reload the
latest document, mutate one stable-ID aggregate, and atomically replace the JSON. Selection and
username updates therefore cannot remove another process's newly added connection or overwrite a
different connection's settings. Edit/delete compare the expected aggregate and surface conflicts.
The UI uses nonblocking lock attempts with a strict 100 ms deadline and returns a typed busy error
when another Deck process still owns the lock, so no config operation can freeze the event thread.
Malformed existing JSON is fail-closed and never replaced by demo defaults. The shared settings
and connections writer uses a unique `create_new` same-directory file, file sync, atomic rename,
and parent-directory sync; stale full snapshots are rejected by content fingerprint.

## Observability and client experience

`logging/mod.rs` owns the process-lifetime `arcen-observability` installation.
Its canonical sink is bounded, rolling JSON Lines under the existing Deck log
directory; debug builds additionally use human-readable stderr. The persisted
UI choices map without index migration to the shared cumulative profiles:
`Level 0 Critical`, `Level 1 Error`, `Level 2 Info`, and `Level 3 Debug`.
Release defaults to Level 0 and debug development defaults to Level 2.
`ARCEN_LOG` refines diagnostics only; typed mandatory lifecycle records bypass
that filter. Shutdown emits `CLIENT_STOP`, waits a bounded interval for sink
delivery, and flushes the shared rolling writer.

Every real or smoke connection carries one Session Log ID through authentication,
`ClientHello`, tracing spans, and canonical lifecycle records. Deck emits the
shared 1500–1506 client lifecycle family plus bounded health, network, HID, and
permission events with explicit user/host/peer context and reason classes.
Messages, credentials, media, HID reports, and raw errors are never lifecycle
fields. TCC granted/denied lifecycle records are deduplicated once per process.

`ClientTelemetry` is caller-owned and lock-free. Decode, presentation, input
handoff, drop, and RTT paths update atomics only; no frame, packet, report, or
input-event log formatting occurs on a hot path. The existing health ping/pong
sequence measures application RTT. A five-second Level 2 snapshot carries PR3
integer client QoS and health to the Pier, a sixty-second Level 0 snapshot is
proof-of-life. Health uses decoded/presented deltas and freshness from the
actual five-second window, with shared two-window hysteresis; cumulative totals
remain only for the final session summary. That summary also records average
FPS/RTT, worst health, and reconnect count. Profile reloads never own or stop
the independent process proof worker.

`netinfo.rs` probes the selected source interface using macOS/Unix APIs without
location prompts or third-party dependencies. `ClientHello.network_snapshot`
and subsequent health pings report only available interface kind, LAN/WAN
scope, and MTU. SSID is omitted by default even when the OS could disclose it;
missing facts remain absent rather than invented.

## Pointer lock and cursor authority

The persisted Advanced preference defaults to Local and is repeated in
`AuthResponse` and `ClientHello`. Deck keeps its synthetic crosshair until the
current connection returns a matching `CursorModeResult`; a request alone never
hides local authority. The crosshair is painted only in active Local mode and
outside pointer lock.

The View-menu Pointer Lock command requires viewer focus plus server input-v2
relative capability. egui 0.35 native `MouseMoved` motion is accumulated without
allocation and emitted once per frame; no edge warping is used. Relative button
and wheel messages are marked Relative so hosts do not prepend an absolute
move. F9, focus loss, disconnect, reconnect detach, and final teardown release
held input, clear fractional residuals, release the viewport grab, restore
cursor visibility, and resume absolute motion. Cursor preference is part of the
reconnect topology binding; each attachment revalidates capability/result state.

## Direct session auto-reconnect

Deck's controller is memory-only and bound to connection generation, endpoint,
TLS/security mode, and display topology. Opt-in creates one random holder nonce;
an initial grant is accepted only as a complete nonzero grant/window pair.
Reconnect options are credential-free, and manual disconnect, configuration
replacement, TLS identity change, protocol/auth failure, deadline, or invalid
successor response zeroizes the state and returns to Home or credentials.

On selected transient EOF/timeout/socket failures, Deck drops the old command
channel and media worker, releases remote keys, but deliberately retains the
last uploaded texture. The reconnect overlay draws that frozen frame dimmed
while randomized exponential retry runs, capped at five seconds and bounded by
the exact deadline established from the transport task's monotonic first-loss
observation, even if UI polling was suspended. A resume task receives that
absolute monotonic deadline (and therefore only the remaining budget) and
applies one timeout to the complete TLS/transport/authentication handshake; both task
and controller cancel at exact equality. Abrupt TCP resets
reported as a missing WebSocket close handshake are transient EOF. While connected,
Deck has no detach deadline: it accepts strict in-band `AuthResult` refreshes
every host-selected `W/2`, zeroizes/replaces the one grant, and records the
current Session Log ID without another acceptance pause. Failed resume
transports preserve the original deadline. Each attempt establishes a fresh
QUIC connection and carries a fresh Session
Log ID while retaining `previous_sid` only for diagnostic chaining.

A resume has empty username/password credential fields and cannot display a new
disclaimer or fall back to ordinary authentication. Deck accepts
`resumed=true` only with a nonempty already-rotated successor grant and nonzero
bounded window before acknowledging authentication. It then installs a fresh
media inbox, VideoToolbox decoder, and audio player, requests a full frame, and
keeps keyframe gating active until fresh media replaces the frozen texture.

The owning cross-component contract is
[`../../docs/architecture/session-auto-reconnect.md`](../../docs/architecture/session-auto-reconnect.md).
Graduation requires Shared/Architecture, Release/Security, macOS Client,
Windows Host, and Linux Host review.

## Clipboard redirection

The persistent Clipboard setting defaults enabled. The real ClientHello builder
advertises exact clipboard version 1 and text/image direction bits only while
enabled; local off advertises version zero and starts no pasteboard polling.
Host policy remains authoritative and the UI displays the effective
direction/content/size.

Transport demultiplexes frame byte `0x20` before the unchanged media inbox.
Inbound reassembly stays off the UI thread and feeds one latest slot. Outbound
state is one active transfer plus one replaceable pending item; the transport loop
sends at most one clipboard chunk per scheduling turn. Reconnect, detach,
endpoint replacement, and terminal teardown replace the generation-scoped
session object, scrub slots, and stop AppKit access before a stale worker can
inject or send.

`NSPasteboard` is accessed only from the eframe/AppKit main thread at 250 ms
intervals after negotiation. String is preferred, then PNG; TIFF is converted
locally through bounded `NSBitmapImageRep`. Remote writes clear the pasteboard,
write private UTI `tech.arcen.clipboard-origin` first, then eager UTF-8 or PNG.
No `arboard`, files, HTML, RTF, delayed rendering, or payload logging is used.

## Tablet / pen input (typed local termination)

Full design: [`../../docs/architecture/pen-tablet-input.md`](../../docs/architecture/pen-tablet-input.md).
Operator validation: [`../../docs/operations/pen-tablet-input.md`](../../docs/operations/pen-tablet-input.md).

The default, non-experimental capture path is the `tablet/` module (see
"Module map" above), not the quarantined `hid/` raw-HID passthrough. A
main-thread AppKit local `NSEvent` monitor (`tablet/monitor.rs`) delivers
typed `tabletPoint`/`tabletProximity` samples; a pure mapper
(`tablet/mapper.rs`) converts them to `arcen_input::PenEvent`, mapping window
points through the current video `image_rect` (accounting for AppKit's
bottom-left origin, backing scale, letterboxing, and fullscreen transitions);
a bounded, edge-preserving dispatcher (`tablet/dispatch.rs`) forwards
`arcen_protocol::messages::PenEventMsg` once the host negotiates input
protocol v3 and pen capability. No Wacom SDK or vendor HID report parsing is
used — Wacom's own driver remains the local decoder, already installed on
this Mac.

`tablet_input_enabled` (default `true`, see `clients/macos/SETTINGS.md`)
gates client pen-capability advertisement; the persisted **View > Tablet
Monitor** panel toggle (`clients/macos/src/ui/macos_menu.rs`,
`ui/app.rs::paint_tablet_monitor_panel`) shows live device/negotiation/tool
state, is local-only, and never logs or persists coordinates, pressure, or
report content. Mouse-emulation duplicates are suppressed only while a real
pen/eraser holds authority; disabling the setting, losing window focus,
leaving proximity, or session teardown/reconnect all restore ordinary mouse
handling and release any mid-contact pen state.

ExpressKeys and the touch ring/strip are configured in Wacom Center on the
client (not by Arcen) to emit ordinary keyboard/wheel input, which Deck
already transports. Tablet multi-touch is not transported. Only one active
tablet/tool per session is supported. Neither Linux nor Windows requires a
Wacom driver — see the host architecture documents for their respective
`uinput`/synthetic-pointer injection backends.

## Signing and distribution

Deck is not sandboxed. `build-deck-app.sh` has three explicit, mutually
exclusive modes and never discovers a keychain identity automatically:
unsigned (default), `--dev-sign` (external Apple Development identity/profile,
verified team/bundle/entitlements, never notarized), and `--release` (external
Developer ID identity/profile/notary profile; CMS-trusted profile, hardened
runtime, notarize, staple, Gatekeeper). `Deck.entitlements` carries only the
identity keys needed to embed a profile plus two profile-authorized
restricted capabilities: `com.apple.developer.sustained-execution` and
`com.apple.developer.associated-domains`. App-Sandbox-only device/network keys
(`com.apple.security.network.client`, `com.apple.security.device.audio-input`)
and the undocumented, TCC-only `com.apple.security.device.input-monitoring`
were removed as no-ops the real profile does not authorize; microphone access
remains TCC-gated through `NSMicrophoneUsageDescription` and the explicit
runtime `AVAudioEngine` consent check in `microphone.rs`, with no entitlement
involved. See `clients/macos/CERTIFICATES.md` for the full cert/profile
inventory and both explicit signing paths.

Capabilities enabled on the `com.example.arcen.deck` App ID but not yet in the
provisioning profile or `Deck.entitlements` (Increased Memory Limit,
Background GPU Access, App Attest, Data Protection) remain pending a
regenerated profile and an implemented use. Network Extensions
(`com.apple.developer.networking.networkextension`,
`com.apple.developer.networking.vpn.api`) and App Groups
(`com.apple.security.application-groups`) are different: the real external
profile already authorizes them today, but they are intentionally not
requested in `Deck.entitlements` because Deck has no Network Extension bundle
or shared-container use yet — see
`clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`.

`validate_release_inputs.py` also enforces an explicit profile class
(`--profile-class release|development`, threaded through both validator
calls in `build-deck-app.sh`) as defense-in-depth independent of the
CMS/team/app/entitlement checks above: a `--release` build rejects a profile
whose `get-task-allow` entitlement is `true` (the standard Apple signal for a
debuggable Development profile, which a Developer ID distribution profile
must not carry), and a `--dev-sign` build requires `get-task-allow: true` so
a Developer ID profile cannot silently be reused in development mode either.

## Deferred / roadmap

- Product message framing remains local. The active dependency-light
  `arcen-transport` default supplies shared TLS lifecycle and pin contracts,
  while its Quinn surface supplies the direct QUIC stream carrier. Dormant WSS
  networking exists only behind `wss-compat`.
- `com.apple.developer.hid.virtual.device` entitlement request to Apple (for future
  macOS Pier, not Deck) — justification text ready in `APPLE_ENTITLEMENT_REQUESTS.md`.
- Windows Pier pen injection: decode Wacom HID reports → `SendInput(POINTER_PEN_INFO)`.
- macOS Pier: `IOHIDUserDeviceCreate` + `IOHIDUserDeviceHandleReport` (pending Apple
  entitlement approval).

## Resume pointer

- **Status:** ✅ MIGRATED + LIVE-VERIFIED (updated 2026-07-31, PR #120). 339 unit tests pass; `Arcen Deck.app` built and used live to stream both Linux Pier (pier-linux.example.internal, 8,881 frames, H.265 4:4:4) and Windows Pier (pier-windows.example.internal, 7,172 frames) over direct QUIC/TLS 1.3 on UDP 18444. Includes the **Retina effective-stream-resolution** fix and the **TOFU certificate trust fix** (QUIC cert capture now correctly promotes to `CertificateUntrusted` for explicit Deck trust). Connections list shows only clean QUIC-only entries; the full-width card button fix lands in this PR. See [`docs/adr/0007-quic-only-product-transport.md`](../../docs/adr/0007-quic-only-product-transport.md).
- **Original next step (done):** establish `clients/macos` as crate
  `arcen-deck-macos`, lib `arcen_deck`, bin `arcen-deck`, using
  `arcen_protocol` via `../../shared/protocol`; the historical migration
  default port was 18443 and the current QUIC product default is 18444,
  `cargo build --release` + `cargo test`, then package `Arcen Deck.app`.
