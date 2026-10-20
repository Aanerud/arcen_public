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
  `nextDrawable`, and a blocked main thread cannot dispatch input.
- Pointer moves and buttons are forwarded in event order, at their own
  positions (`ordered_pointer_events`), not once per pass.
- Key, modifier, scroll and mouse-button events raise the pen ordering
  barrier in the tablet monitor. A key release the input method swallowed
  keeps pen samples held until text input's release recovery has sent it
  (`complete_pen_pass_at`, bounded at 250 ms once per pending release; focus
  loss clears pending recovery). Held pen samples already own
  the pointer (`PenInput::holds_authority`), so their mouse-emulation
  duplicates never reach the host.
- Tests: `pen_samples_at_200_hz_are_forwarded_at_200_hz_while_the_ui_runs_at_30_hz`,
  `samples_after_a_keyboard_edge_wait_for_the_ui_pass_and_keep_their_order`.

Follow-up: keyboard (and secondary-viewport pointer) input still leaves on the
UI pass; a keyboard fast path is a separate package.

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
- Apple entitlement request justifications: `clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`
- Build + sign: `packaging/macos/build-deck-app.sh`
- Entitlements plist: `packaging/macos/Deck.entitlements`

When adding new entitlements: update the plist, check the capability is enabled on the
`deck.arcen.tech` App ID at developer.apple.com, and update `CERTIFICATES.md`.

Escalate shared API or protocol changes to Shared/Architecture; keychain,
permissions, signing, notarization, packaging, and release changes to
Release/Security.
