# ADR 0029: The macOS Pier draws the pointer into the video

**Status:** Accepted 2026-11-22. Applies to the macOS Pier and to every Deck
connected to it. Revisit if macOS gains a public way to read the desktop
pointer's image, or if users on Mac hosts report pointer lag as a problem.

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
   `CGSGetCurrentCursorLocation` and the cursor seed. One reference product
   reads the desktop pointer this way on Mac hosts and draws it on the
   client. These calls are undocumented, can change or vanish with any
   macOS update, and have no qualification or permission contract.
3. **A hybrid.** Draw the standard arrow locally and fall back to the
   in-video pointer for every other shape. This is a guess about which shape
   is showing, so it is wrong whenever an application changes the pointer.
4. **The pointer in the video** (`showsCursor`). Public API under the Screen
   Recording permission the Pier already needs. The shape is always exact.
   The pointer moves with the video, so it can trail the hand by the
   stream's latency. Another reference product's macOS agent documents the
   same trade-off as its host-rendered cursor mode.

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
