# macOS Client Ownership

**Owner role:** macOS Client

Own the macOS client UI boundary, media presentation, input capture, HID device
passthrough (HoIP), client session lifecycle, and macOS packaging coordination in
this path.

Validate on macOS with
`cargo build --locked --release -p arcen-deck-macos` and
`cargo test --locked -p arcen-deck-macos`, plus the root shared-crate test and
strict Clippy gates. There is no single-platform `--workspace` build.

## Streaming and presentation boundaries

- Production exposes exactly four complete presets: Auto, Speed, Grading, and
  HDR. Do not reintroduce independent performance/colour switches as the normal
  user surface; exact axes remain diagnostic/developer controls.
- The ordinary 8-bit UI/video path and the dedicated 10-bit Metal video layer
  are separate presentation pipelines. Do not force Auto/Speed through the
  wide layer or make Grading/HDR depend on the 8-bit egui surface.
- Transfer characteristics decide HDR. Ten-bit BT.709 Grading remains SDR;
  only a host-confirmed PQ stream enables the ITU-R BT.2100 PQ colour space,
  HDR metadata, and EDR.
- Preserve native VideoToolbox colour metadata, including PQ/HLG transfer
  constants. If native 10-bit presentation fails, retain the 8-bit fallback
  only with a persistent visible warning.
- Reconnect creates a fresh decoder/inbox and waits for a fresh keyframe.
  Teardown must discard queued frames rather than let stale data delay resume.

## Presentation timing

When a frame is shown is the served pipeline's shared rule
(`arcen_media::video::PresentTiming`, carried by every `PresentableFrame`
from the shared media worker;
[`docs/architecture/deck-presentation-timing.md`](../../docs/architecture/deck-presentation-timing.md)).
The eight-bit Metal layer renders an Auto frame the moment it arrives
(display sync on, the pacer keeps only the newest; one drawable in flight,
two where the measured submission-to-glass time exceeds a refresh because
the system composes the layer, `arcen_media::NewestPresentDepth`; a frame
waiting for a slot goes the moment a presented handler frees one, from the
presenter's slot thread, and the `CVDisplayLink` tick is the retry path),
and Speed also turns
`displaySyncEnabled` off, keeps two drawables in flight and presents without
a minimum duration, so it may tear. Grading and HDR keep the `CVDisplayLink`-paced ten-bit layer with
display sync, EDR and colour handling unchanged. Do not add a local preset
or tearing rule here: change the shared contract. Fullscreen draws nothing
over the video: the system menu bar auto-hides and drops down at the top
edge.

## Input never waits for the UI pass

Pen samples leave from the AppKit monitor callback the moment they are
dispatched (`ui/pen_input.rs`); the UI pass only publishes routing and drains
what the monitor had to queue. Keep it that way:

- Do not move pen forwarding back into `App::ui`/`logic`, and do not make pen
  motion request a repaint (edges only).
- AppKit mouse coalescing stays off only while a session window is active
  (`tablet::MouseCoalescing`); restore it on deactivate and quit.
- The UI never asks for passes faster than the root display refreshes
  (`paced_ui_poll_interval`): a faster request blocks the main thread in
  `nextDrawable`, and a blocked main thread cannot dispatch input. Hand
  every delayed repaint to egui through
  `arcen_deck_core::ui_pacing::egui_repaint_delay`: egui subtracts its
  predicted frame time (1/60 s), so a bare `request_repaint_after(16 ms)` is
  an immediate repaint, and with display sync off (multi-window) nothing
  caps it (786 passes/s measured).
- Passes egui asks for itself (two settle passes after every input pass)
  are paced at the event loop: `ui/pass_pacing.rs` sits between winit and
  eframe (`run_native_app` uses `eframe::create_native`) and holds a root
  `RedrawRequested` until one root refresh after the previous root pass
  began (shared `arcen_deck_core::ui_pacing::PassPacer`; resize and scale
  changes run at once). Input events reach egui at once; root pointer and
  keys leave on the next pass, at most one refresh later, as with display
  sync on. A pass the media worker asks for (`pass_pacing::MediaWake`) is
  never held: in multi-window the root pass hands out every window's
  decoded picture. Secondary windows are not paced there.
- Pointer moves and buttons are forwarded in event order, at their own
  positions (`ordered_pointer_events`), not once per pass.
- The root window's pointer leaves from `raw_input_hook` as the root pass
  begins, before the UI runs (`forward_root_pointer_events`), from the
  viewer's place in the pass just before (`RootPointerGate`); the viewer
  pass only focuses the viewer after a press. Under pointer lock or a pen's
  authority, without a gate from the previous root pass, and in a pass that
  also carries a key, text or a modifier change, the viewer pass forwards
  them as before, so a shift-click or a typed letter keeps its place before
  the click. Test: `root_pointer_events_leave_before_the_ui_pass_unless_keys_came_with_them`.
- A secondary Match My Layout viewport's pointer leaves from
  `raw_input_hook` as that viewport's pass begins
  (`forward_secondary_pointer_events`, shared
  `arcen_deck_core::input::viewport_pointer`), never from root's pass:
  egui runs two settle passes after every input pass, so a per-paint
  sample read later by root is overwritten (4-7 of 120 moves and lost
  releases measured). eframe calls `raw_input_hook` for every viewport;
  anything root-only there must check `raw_input.viewport_id`.
  Test: `every_secondary_pointer_event_reaches_the_host_without_a_root_pass`.
- Key, modifier, scroll and mouse-button events raise the pen ordering
  barrier in the tablet monitor. A key release the input method swallowed
  keeps pen samples held until text input's release recovery has sent it
  (`complete_pen_pass_at`, bounded at 250 ms once per pending release; focus
  loss clears pending recovery). Held pen samples already own
  the pointer (`PenInput::holds_authority`), so their mouse-emulation
  duplicates never reach the host.
- Tests: `pen_samples_at_200_hz_are_forwarded_at_200_hz_while_the_ui_runs_at_30_hz`,
  `samples_after_a_keyboard_edge_wait_for_the_ui_pass_and_keep_their_order`.

Follow-up: keyboard and scroll (and secondary-viewport keys and scroll,
gathered until root's next pass, which they wake) still leave in the UI
pass, and a pointer move still costs a root pass (at most one per
refresh). Sending pointer and keys from the AppKit monitor as they are
dispatched, as pen does, with plain moves over the video kept from egui,
is a separate package.

## Native tablet: one path

While Native tablet is active and the host leads the cursor (a bridged pen
moved the host pointer), the Deck draws the pen dot (white, dark ring) at
the host position on every host, whatever the host's cursor image is
(`arcen_input::host_pointer::led_pointer_mark`, ADR 0023). The Mac's own
mouse takes the cursor back the moment it moves. The `native-tablet` feature
is the helper and bridge (the old `usb-hard-lab` name is gone);
`usb-emulate-lab` stays lab-only test tooling. Emulated strokes prove
plumbing, not a real pen: check a real tablet in Chrome (Safari reports
every pen as a mouse).

## Following a sign-in screen into the desktop

A host that serves its operating system's sign-in screen says so in its hello
(`ServerHelloMsg::login_window`). Signing in there ends that session by
design. The Deck therefore treats that close as a hand-over, not a lost
connection. This behaviour is agreed and measured on the lab Mac: about 4.5 s
from sign-in to the new desktop's hello. Keep it:

- Credentials outlive the hello **only** for a sign-in screen
  (`note_hello_desktop`). A signed-in desktop's hello, a manual disconnect, and
  running out of attempts all drop them. `AuthSubmission` wipes itself on drop.
- On close, `follow_login_window` holds the last frame under "Signing in…" and
  reconnects on the shared schedule (`arcen_session::login_window_handover`:
  2.5 s, then 2 s, five attempts). It never follows a manual disconnect or a
  TLS identity change.
- `start_connection` calls `disconnect()`, which drops the hand-over, so
  `drive_login_window_handover` carries it across the start. An attempt that
  lands on a sign-in screen again keeps counting; it does not restart.
- Tests: `a_closed_sign_in_screen_is_followed_holding_the_last_frame` and
  `only_a_sign_in_screen_keeps_credentials_past_its_hello`. Change the
  behaviour only on purpose, and change these tests with it.

## Signing and certificates

- Cert inventory and dev-machine setup: `clients/macos/CERTIFICATES.md`
- Entitlements each app holds, and why: `clients/macos/ENTITLEMENTS.md`
- Build + sign: `packaging/macos/build-deck-app.sh`
- Entitlements plist: `packaging/macos/Deck.entitlements`

When adding new entitlements: update the plist, check the capability is enabled on the
`deck.arcen.tech` App ID at developer.apple.com, and update `CERTIFICATES.md`.

Escalate shared API or protocol changes to Shared/Architecture; keychain,
permissions, signing, notarization, packaging, and release changes to
Release/Security.
