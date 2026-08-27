# Typed pen/tablet input — operator validation

**Status (2026-08-03):** Physical end-to-end validation completed. A Wacom
Intuos5 touch L pen was physically moved, pressure-applied (range 0.0–1.0
confirmed), tilted, and clicked through Deck's Tablet Monitor, with 1,572+
forwarded pen events confirmed reaching both Linux (pier-linux.example.internal, uinput path)
and Windows (pier-windows.example.internal, PT_PEN path) hosts. The human-physical test gate
below is now cleared. See
[`../architecture/pen-tablet-input.md`](../architecture/pen-tablet-input.md)
for the full design, the AppKit event gate fix (2026-08-03), and the
confirmed capability coverage table.

**Note for re-validation**: A critical macOS capture bug was found and fixed
during this run (commit `1d6af87`). Wacom delivers pen motion with `vendorID=0`
on macOS; the original gate dropped all such events. Any re-test must use
Arcen Deck build `1d6af87` or later.

## Prerequisites

- A Wacom tablet (validated against `Intuos5 touch L`, VID `0x056A` PID
  `0x0317`) connected to the macOS client, with Wacom's own driver suite
  installed and running.
- An Arcen Pier reachable over a direct QUIC connection: Linux
  (`arcen-pier` on the dedicated per-session Xorg target — see
  `hosts/linux/ARCHITECTURE.md`) or Windows (`arcen-pier.exe` as the
  LocalSystem SCM service).
- The saved connection set to **Tablet support** (the default,
  `tablet_mode_requested = local_termination`; see
  `clients/macos/SETTINGS.md`) unless a step below explicitly switches to
  **Mouse compatibility only** to exercise fallback behavior.

Bridge mode operational note: `wacom_usb_bridge` is visible in Deck settings,
but hosts without a complete USB bridge backend return explicit
`tablet_mode_result.accepted = false` plus a reason; operators should expect
`active = disabled_mouse_compat`. Native mode is explicit and never causes an
implicit switch to Tablet support.

## 1. macOS: presence, negotiation, and the Tablet Monitor panel

1. Launch `Arcen Deck.app` (or `arcen-deck` CLI) with the tablet attached and
   connect to a Pier. Confirm the startup log includes the Wacom vendor ID
   (`0x056A`) in the USB inventory line
   (`arcen_deck::logging::diagnostics::log_usb_inventory`).
2. Open **View > Tablet Monitor**. Confirm:
   - `Wacom USB presence: present`.
   - `Runtime installed: true` (the AppKit local-event monitor installed on
     the main thread).
   - `Negotiated: yes (typed pen)` once connected to a Pier that advertises
     input v3 and `pen = available`. Against an older/mouse-only Pier or
     with the setting off, confirm the panel truthfully reports
     `no (mouse-emulation fallback; pressure unavailable)` or
     `no (setting disabled)` instead — never a false `yes`.
3. With the pen physically moved over (not yet touching) the tablet, confirm
   `in proximity: true`, `touching: false`, and the empirical probe fields
   (`pressure`, `tilt`, `rotation`, `eraser`, `barrel`) move from `Unknown`
   toward `Available` as each axis is actually exercised — never claim these
   before they are observed.
4. Press the tip to the tablet with varying pressure; confirm `Pressure:`
   changes continuously and `touching: true` while in contact.
5. Tilt the pen; confirm `tilt X°/Y°` changes. Rotate a tool that reports
   barrel rotation (if available); confirm `rotation` changes. Flip to the
   eraser end; confirm `Tool: eraser`. Press each barrel button; confirm the
   event stream reflects both bits independently.
6. Confirm `Events sent` increases, `overflow resets`/`Producer dropped`
   stay at 0 during normal interactive use, and `Last error: -` (no error)
   throughout.
7. Move the mouse while the pen is in proximity; confirm the mouse cursor
   does not duplicate/fight the pen's motion (mouse-emulation suppression).
   Move the pen out of proximity; confirm ordinary mouse control returns
   immediately.
8. **ExpressKeys/ring:** in Wacom Center (Mac), map ExpressKeys and the touch
   ring/strip to ordinary keys or a scroll/zoom shortcut, then confirm those
   keypresses/scroll events arrive at the remote session through Deck's
   existing keyboard/wheel path — Arcen has no separate ExpressKeys protocol.
9. **Tablet touch:** confirm multi-touch gestures on the tablet surface (not
   the pen) produce no `PenEventMsg` traffic and no remote effect — touch is
   intentionally bridge-only/unsupported here.

## 2. macOS: signed dev/release mode and TCC distinction

These validate that the default typed-pen path needs no protected
entitlement, and that the two explicit signing modes remain distinct — see
`clients/macos/CERTIFICATES.md` and `clients/macos/ARCHITECTURE.md` →
"Signing and distribution" for the authoritative detail (not duplicated
here).

1. **Unsigned/default build** (`packaging/macos/build-deck-app.sh`, no
   flag): confirm typed pen capture and negotiation above work with **no**
   System Settings > Privacy & Security > Input Monitoring entry required —
   default in-window `NSEvent.tabletPoint`/`tabletProximity` delivery does
   not need it. Input Monitoring only matters to the separately quarantined,
   non-default experimental raw-HID path.
2. **Development mode** (`--dev-sign` with `ARCEN_DEV_CODESIGN_IDENTITY` /
   `ARCEN_DEV_PROVISIONING_PROFILE`): confirm the app signs with an Apple
   Development identity, is never notarized, and typed pen behaves
   identically to the unsigned build.
3. **Release mode** (`--release` with `ARCEN_CODESIGN_IDENTITY` /
   `ARCEN_PROVISIONING_PROFILE` / `ARCEN_NOTARY_KEYCHAIN_PROFILE`): confirm
   the resulting `.app` is Developer ID signed, notarized, stapled, and
   passes Gatekeeper assessment (`spctl --assess`), and that typed pen
   behaves identically. Do not report `--release` evidence unless this exact
   flow actually ran — a `--dev-sign` build is not release evidence.
4. Confirm `packaging/macos/validate_release_inputs.py --profile-class
   release|development` rejects a profile/class mismatch (a `get-task-allow:
   true` profile under `--release`, or `get-task-allow: false` under
   `--dev-sign`) before signing.

## 3. Linux: libinput/evtest and a creative application

Run on the target Linux Pier's dedicated per-session Xorg display (not the
shared root console) after a session with a negotiated pen is attached.

```bash
# Confirm the kernel sees a distinct virtual tablet-tool device (not merged
# into the mouse device) and that it classifies as a tablet tool:
libinput list-devices | grep -A5 "Arcen Virtual Tablet Pen"
libinput debug-events --device /dev/input/by-id/<arcen-virtual-tablet-pen-event>

# Or, low-level per-event inspection:
sudo evtest
# select "Arcen Virtual Tablet Pen" from the listed devices
```

1. While moving/pressing the physical pen client-side, confirm
   `libinput debug-events` (or `evtest`) reports `TABLET_TOOL` proximity,
   motion, tip, pressure, and tilt events on the Arcen virtual device — and
   that the *existing* Arcen mouse/keyboard uinput device shows no tablet
   axes (confirming the two devices stayed separate).
2. Confirm no rotation axis is advertised (`libinput list-devices` should
   not show a rotation capability for this device) — this is the documented
   `Unavailable` state, not a bug.
3. Open **Krita** (or another approved Linux creative test application) in
   the same dedicated Xorg session and confirm a brush stroke shows varying
   width/opacity as physical pressure changes, and that tilt-sensitive
   brushes respond to physical tilt.
4. Confirm **no Wacom driver package** is installed or required on this
   Linux host for the above to work.
5. Disconnect the client (or disable `tablet_input_enabled`) mid-stroke;
   confirm the virtual device releases every held key/button/proximity state
   (no stuck `BTN_TOOL_PEN`/`BTN_TOUCH`/`BTN_STYLUS*`) — re-run
   `evtest`/`libinput debug-events` immediately after and confirm no held
   state is reported.

## 4. Windows: Windows Ink surface

1. Open a Windows Ink-aware test surface — for example the Windows Ink
   Workspace's Sketchpad, or any application built on the `RealTimeStylus`/
   `Pointer` APIs (`WM_POINTER`/`GetPointerPenInfo`) — while a session with a
   negotiated pen is attached.
2. While moving/pressing the physical pen client-side, confirm the surface
   reports `PT_PEN` pointer type, continuously varying pressure (mapped to
   the documented `0..=1024` range — do not expect or claim 8192 levels),
   tilt, and rotation where the drawing tool provides it.
3. Flip to the eraser end; confirm the surface reports the eraser flag.
   Press a barrel button; confirm the surface reports it as the pen barrel
   button.
4. Confirm hover (in-range, not in contact) is visually distinct from
   contact (tip down), and that the injected coordinates land at the
   correct point on the session's selected display.
5. Confirm **no Wintab-only application** is expected to receive input here
   — this backend is Windows Ink/`WM_POINTER` only; Wintab support is not
   claimed.
6. Force `CreateSyntheticPointerDevice` to fail (e.g. run on an
   unsupported/older Windows build, or with Windows Ink disabled via Group
   Policy) and confirm the Pier advertises every pen capability as
   `Unavailable` in `ServerHello`, the attachment proceeds mouse-only, and
   no partial/fake pressure is reported.
7. Disconnect mid-contact; confirm proximity/contact is released (no stuck
   pointer state) and a subsequent reconnect creates a fresh, clean
   `PenInjector`.

## 5. Old-peer fallback and default-disabled raw-HID

1. Connect Deck to a Pier build that predates `input_protocol_version = 3`
   (or a build where the pen device failed to create): confirm the
   `ServerHello`'s pen capability fields are absent/`Unknown`/`Unavailable`,
   Deck's Tablet Monitor reports the honest non-negotiated state, and
   ordinary mouse/keyboard behavior is completely unaffected — no crash, no
   partial pen behavior, no protocol error.
2. Confirm a default build (no `experimental-raw-hid` Cargo feature, no
   `ARCEN_EXPERIMENTAL_RAW_HID` environment variable) never starts the
   quarantined raw-HID path and never advertises
   `experimental_raw_hid: true` in `ClientHello`/`ServerHello`, regardless of
   what the other peer sends.
3. Confirm a Windows Pier never advertises `experimental_raw_hid: true`
   under any configuration (it has no raw-HID receiver at all).

## 5a. Measured status of experimental raw-HID passthrough (2026-08-12)

The two halves of this feature are in very different states. Measured, not
inferred.

### Client half: works, and needs no Apple entitlement

An Intuos5 touch L (`056a:0317`) is captured with the Wacom driver installed
and running and no entitlement of any kind. Reproduce with the hardware test,
which drives the real `HidSession` rather than a probe that re-implements it:

```sh
cargo test -p arcen-deck-macos --features experimental-raw-hid \
    --test tablet_capture_hardware -- --ignored --nocapture
```

Observed: `PermissionGranted`, `DeviceAdded vid=0x056a pid=0x0317
descriptor_len=234`, then live pen `Report`s. Input Monitoring must be granted
to whatever runs it.

`kIOReturnExclusiveAccess` from `IOHIDManagerOpen` is **transient** — another
process holding the device — not a structural bar. It was seen against every
matching combination on one occasion and against none of them after the Wacom
driver was restarted. Restart the vendor driver or replug the tablet.

### Host half: blocked, and by design it always was

A `/dev/uhid` device created with Wacom's vendor ID binds **no driver at all**
on RHEL 9, so no input device appears and there is no pen. `wacom.ko` is a USB
driver and a uhid device is not a USB device.

This is not a defect to fix here. It is the answer to the open empirical
question in
[`../todo/input-only-usb-bridging.md`](../todo/input-only-usb-bridging.md),
which already separates the implemented typed path ("Light") from the future
native USB bridge ("Hard") and already states that the experimental raw-HID
path is lab scaffolding that must not grow into the bridge. The full
measurement, the reference's `usb-vhci` architecture, the out-of-tree kernel
module cost, and the reopened client entitlement question are all recorded
there — that document is authoritative; do not re-derive it from this one.

Operationally, for anyone testing today: **raw-HID passthrough cannot deliver a
working Wacom on a Linux Pier**, so do not enable it expecting one. Use the
typed pen path, which is the supported configuration.

## 5b. Open: tilt reaches the Linux host but is not observed by applications

Reported 2026-08-12: pen tilt works on a Windows Pier and not on a Linux Pier.
Not investigated further at the owner's direction; this records the measurement
so the client side does not get re-examined needlessly.

Tilt **does** arrive at the Linux host, with live varying values. From
`/var/log/arcen/arcen-pier.log` with `ARCEN_LOG=input=trace`:

```text
pen_inject: ... "tilt_x":"71.71819305419922","tilt_y":"-4.218878746032715",
            "tool":"Tip","in_proximity":true,"touching":false,"source":"region"
pen_inject: ... "tilt_x":"73.12448120117188", ...
pen_inject: ... "tilt_x":"70.31189727783203", ...
```

So AppKit capture, `tilt_to_degrees`, `PenEventMsg` and the host's injection
path are all carrying tilt correctly, and `create_pen_device` declares
`ABS_TILT_X`/`ABS_TILT_Y` with a matching `AbsInfo` range. The loss is
**downstream of `uinput` emission** — the tablet-tool device's XI2 valuators,
the X server's view of them, or the application. Start there, not at the client.

Note the host writes these traces to `/var/log/arcen/arcen-pier.log`, **not** to
the journal; `journalctl -u arcen-pier` shows none of them, which is misleading
enough to have cost a diagnosis.

## 6. Teardown and logs/privacy

1. During an active pen contact, force each of: focus loss, disconnect,
   terminal session teardown, and disabling `tablet_input_enabled`. After
   each, confirm (per platform) that no key/button/proximity state remains
   held on the injected device, and that a fresh reconnect starts clean.
2. Inspect Deck, Linux Pier, and Windows Pier logs from a validation run.
   Confirm they contain only lifecycle/counter facts (e.g. "pen device
   created", "pen event count", negotiated capability booleans) and **never**
   coordinates, pressure samples, tilt/rotation values, raw report bytes, or
   any other tablet input content — matching the Tablet Monitor panel's own
   "never logged or persisted" rule.
3. Run a Support Bundle collection (`arcen-pier support-bundle` on either
   host) during/after a pen session and confirm it contains no raw pen
   sample content, consistent with its existing strict allowlists.

## Known human-physical test gate

Every step above can be exercised through enumeration, synthetic samples, or
a real pen — but **completion of this feature requires an actual person
physically moving and pressing a real Wacom pen** against the attached
tablet, observed live through Deck's Tablet Monitor panel (section 1) and
confirmed to reach a real remote creative application on both Linux (section
3) and Windows (section 4). Do not report this feature as fully validated,
and do not claim a physical end-to-end run occurred, until that exact
physical interaction has actually happened and been recorded by whoever
performed it.
