# ADR 0029: The macOS Pier draws the pointer into the video

**Status:** Accepted 2026-11-22. Amended 2026-11-23 (amendment 1): the
premise was wrong, the macOS Pier can read the desktop pointer's image, and
it now sends it; the pointer in the video remains only as the fallback.
Applies to the macOS Pier and to every Deck connected to it.

> Records why a Deck connected to a macOS Pier shows the host's pointer in
> the video instead of drawing its own, what the other routes were, and why
> they were not taken.

## Context

Every Deck asks for the automatic cursor. The shared rule
(`arcen_input::resolve_automatic_cursor_mode`) decides who draws the
pointer. A Pier that sends cursor images gets a locally drawn pointer: the
Deck draws the shape at its own position, so the pointer moves without
waiting for the video. A Pier that sends no cursor images draws the pointer
into the video, and the Deck hides its own pointer over the stream
(`CursorModeResultMsg::host_draws_pointer`).

The Linux Pier (XFixes, Mutter's screencast cursor metadata) and the
Windows Pier (`GetCursorInfo`) read the desktop pointer's image and send
it. The macOS Pier sends none (`HOST_SENDS_CURSOR_IMAGES` in
`hosts/macos/src/session.rs`) and turns on ScreenCaptureKit's
`SCStreamConfiguration.showsCursor`, so the compositor draws the pointer
into every captured frame. The difference is recorded as a platform
difference in `scripts/ci/platform-differences.toml`.

Seen on a lab Mac on 2026-11-22: every session resolved to `Host`,
`automatic`, `host_draws_pointer`. The pointer's shape was always right,
and it moved with the video.

## Routes considered

1. **A public cursor image.** `NSCursor.currentSystemCursor` returns the
   calling process's own cursor, not the desktop's. This was measured, not
   assumed: `hosts/macos/src/cursor_probe.rs`. No other public API gives
   the desktop pointer's image. Apple's own direction for capture is
   `showsCursor`.
2. **Private CoreGraphics calls**, such as `CGSGetGlobalCursorData`,
   `CGSGetCurrentCursorLocation` and the cursor seed. These calls are undocumented, can change or vanish with any
   macOS update, and have no qualification or permission contract.
3. **A hybrid.** Draw the standard arrow locally and fall back to the
   in-video pointer for every other shape. This is a guess about which shape
   is showing, so it is wrong whenever an application changes the pointer.
4. **The pointer in the video** (`showsCursor`). Public API under the Screen
   Recording permission the Pier already needs. The shape is always exact.
   The pointer moves with the video, so it can trail the hand by the
   stream's latency.

## Decision

The macOS Pier keeps route 4: it sends no cursor images and draws the
pointer into the video. The Deck hides its own pointer over a Mac host's
stream. Arcen does not use private CoreGraphics calls for the pointer.

## Consequences

- One pointer is visible, and it is always the right shape.
- On a Mac host the pointer moves with the video. On Linux and Windows
  hosts it is drawn by the Deck and moves at once.
- The shared cursor-image path (`arcen_input::cursor`,
  `arcen_protocol::cursor`, both Decks' drawing) stays ready, so a macOS
  image source can be added later without protocol changes.
- Two pointers on screen over a Mac host is a Deck bug, not a property of
  this decision.

## Amendment 1 (2026-11-23): the pointer's image can be read

Users on Mac hosts found the pointer laggy: on two monitors at 30 frames a
second every movement waited for capture, encoding, the network and the
Deck's display. That is the trigger this ADR named, so route 1 was measured
again.

### What was measured

`arcen-pier-macos probe-cursor` (now with `--passive`, `--pump` and
`--seconds`) on a Mac running macOS 26.7.3, signed in at the console, with a
terminal window frontmost under the pointer. The sweep moves the pointer in
a lap for five seconds and puts it back; it ran only after the Mac had seen
no input for two minutes. A Swift harness made the same reads to separate
the run loop from the thread.

| Run | Main thread between samples | Distinct images | Images |
| --- | --- | --- | --- |
| passive, 3 s | sleeps | 1 | 9x18 I-beam, hotspot 4,9 |
| passive, 3 s | runs its run loop (`CFRunLoopRunInMode`) | 1 | the same I-beam |
| sweep, 5 s | sleeps | 3 | I-beam; 28x40 arrow; 18x28 up-down resize |
| sweep, 5 s | runs its run loop | 3 | the same three |
| sweep, 4 s (Swift) | sleeps | 4 | I-beam; arrow; left-right and up-down resize |
| sweep, 4 s (Swift) | runs its run loop | 4 | the same four |
| sweep, 4 s (Swift), read on a background thread | sleeps | 4 | the same four |

The image was drawn into an sRGB bitmap and checked by eye (the I-beam, the
arrow with its shadow, the resize arrows), at one and two pixels per point.

`currentSystemCursor` keeps answering the last image while an application
hides the pointer, so visibility was measured separately, from a
background process, while a test application in front hid the pointer:

| What the application in front did | `CGCursorIsVisible` in the probe | Watcher records |
| --- | --- | --- |
| typed three keys into its text view (posted to its own process only) | false until it lost focus (53 of 89 samples hidden) | image, hidden, image |
| `NSCursor.hide`, then `unhide` | false, then true | hidden, image |
| `setHiddenUntilMouseMoves:` yes, then no | false, then true | hidden, image |
| `CGDisplayHideCursor`, then `CGDisplayShowCursor` | false, then true | hidden, image |

The watcher records are the real `CursorUpdate` records the session sends,
decoded by `probe-cursor` (`watcher_records`). Moving the pointer onto the
second display and back gave records on output 2, then 3, then 2: the
display's id is the record's output, and the image is drawn at that
display's scale. Both displays of this Mac run at one pixel per point, so
the two-scale case is covered by the shared test
(`output_under_pointer`) and the 2x bitmap check above, not by hardware.

### Why the earlier measurement was wrong

`NSCursor.currentSystemCursor` reports the desktop's cursor whichever
process set it; it is not the calling process's cursor. The probe and the
session's watcher named the shape by comparing the image's bytes with
`NSCursor`'s class cursors, and the window server's copy of the I-beam
(9x18 points, two representations) and of the edge-resize cursors never
byte-match AppKit's (the class I-beam is 23x22 points, four
representations). Every unmatched image was reported as the default arrow,
so every cursor looked like the arrow. The probe's own `distinct_images`
counter, and the session log's `cursor_images_seen` (86 images against 59
named, recorded in 2026-09), showed the images changing all along.

Not the cause: the agent never runs its main run loop (Tokio's
`block_on` owns that thread). Pumping it changed nothing above, and reading
from a background thread while the main thread sleeps — what the agent does
— reads the same images. The agent's threading is unchanged.

### Decision

The macOS Pier sends the desktop cursor's image, as the Linux and Windows
Piers do, through the same wire record and shared code:

- `cursor_probe::CursorWatcher` polls `currentSystemCursor` and
  `CGCursorIsVisible` every 60 ms. A hidden pointer is offered as hidden
  until it moves: the window server ends a `setHiddenUntilMouseMoves:` hide
  (Chrome's on every key typed into a page) only on hardware motion, never
  on the session's posted motion (measured 2026-11-23), so a pointer that
  moves while hidden with no mouse button held is offered as shown
  (`HiddenUntilMoved`); a key typed starts the wait over, and a drag never
  counts.
  A shown one is drawn into straight-alpha RGBA, with its hotspot, at the
  scale of the session display the pointer is on (shared
  `arcen_input::cursor::output_under_pointer` over `PointerOutput`s:
  stream pixels per desktop point; output 0 for a single display, the
  display id for each display of Match My Layout) and drawn again when the
  pointer moves to a display of another scale. Both go to the shared
  `arcen_input::cursor::HostCursor` (stable ids, repeats suppressed, new
  images rate-limited) in that display's pixel space (`set_output`). `arcen_input::cursor::agent_cursor_record` writes
  the `CursorUpdate` record and the service's lane relay
  (`CursorRecordRelay`) puts it on the input/cursor lane, inlining an image
  only while the Deck does not hold it. No protocol change.
- At sign-in the agent offers images when the Deck declared
  `cursor_image_v1` and a live check reads the cursor's image now
  (`cursor_probe::desktop_cursor_images_readable`). The shared
  `resolve_automatic_cursor_mode` then resolves Local for a Deck that asks
  for the automatic cursor: the Deck draws the pointer itself, in the
  desktop's shape, at the speed of the hand, and the capture leaves the
  pointer out.
- **Fallback.** When the check fails (no window-server connection, nothing
  readable), the session behaves exactly as this ADR first decided: Host,
  `showsCursor`, the Deck hides its pointer.
- A Deck that draws no images keeps the named shapes, now as a fallback
  name only.
- Still not used: private CoreGraphics calls (route 2).

### Consequences

- On a Mac host the pointer moves at the speed of the hand, like on Linux
  and Windows hosts, in the Mac's own shapes.
- A pointer the desktop hides (typing, `NSCursor.hide`, a full-screen
  video or game) is hidden on the Deck too.
- On Match My Layout each display shows the pointer at its own size, also
  when the displays' scales differ.
- `currentSystemCursor` and `CGCursorIsVisible` are deprecated without a
  replacement. If a future macOS stops answering the first, the live check
  fails and the Pier falls back to the pointer in the video; `probe-cursor`
  re-checks both (`distinct_images`, `hidden_samples`, `watcher_records`).
