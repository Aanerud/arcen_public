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

**Vendor neutrality (0.16.0):** any pen tablet whose HID report descriptor
declares a pen is supported, whoever made it; no step below depends on the
tablet's brand. The physical run above used one Wacom; tablets of other makes
are covered by classifier and normalisation tests with synthetic descriptors
(`shared/usb-bridge/src/hid_digitizer_tests.rs`,
`shared/input/src/pen_axes.rs`) but have not yet been physically validated —
re-run sections 1, 3 and 4 with any other tablet you have and record it.

**Note for re-validation**: A critical macOS capture bug was found and fixed
during this run (commit `1d6af87`). Tablet drivers deliver pen motion with `vendorID=0`
on macOS; the original gate dropped all such events. Any re-test must use
Arcen Deck build `1d6af87` or later.

## Prerequisites

- A pen tablet of any make connected to the macOS client (USB or Bluetooth),
  with its own macOS driver installed and running if the vendor ships one
  (physically validated so far with a Wacom `Intuos5 touch L`, `056a:0317`).
- An Arcen Pier reachable over a direct QUIC connection: Linux
  (`arcen-pier` on the dedicated per-session Xorg target — see
  `hosts/linux/ARCHITECTURE.md`) or Windows (`arcen-pier.exe` as the
  LocalSystem SCM service).
- The saved connection set to **Tablet support** (the default,
  `tablet_mode_requested = local_termination`; see
  `clients/macos/SETTINGS.md`) unless a step below explicitly switches to
  **Mouse compatibility only** to exercise fallback behavior.

Bridge mode operational note: Native tablet (USB bridged) — wire token
`wacom_usb_bridge`, a legacy name for a vendor-neutral mode — is visible in Deck settings,
but hosts without a complete USB bridge backend (every Windows Pier, by
decision: [ADR 0017](../adr/0017-forward-only-pen-input.md)) return explicit
`tablet_mode_result.accepted = false` plus a reason; operators should expect
`active = disabled_mouse_compat`. Native mode is explicit and never causes an
implicit switch to Tablet support.

## 1. macOS: presence, negotiation, and the Tablet Monitor panel

1. Launch `Arcen Deck.app` (or `arcen-deck` CLI) with the tablet attached and
   connect to a Pier. Confirm the USB inventory line
   (`arcen_deck::logging::diagnostics::log_usb_inventory`) counts the tablet
   (`tablets=1`) and logs `Graphics tablet attached` for it; `brand` is a
   label for a few well-known vendors and reads `unbranded` for any other
   make — it never decides whether the device is a tablet. A Bluetooth tablet
   does not appear in the USB inventory but is still detected below.
2. Open **View > Tablet Monitor**. Confirm:
   - `Pen tablet: present` (from the HID report descriptors in the I/O
     Registry, or from a pen sample AppKit already delivered). This is
     informational: `Typed pen offered: yes` whenever the setting is on,
     tablet attached or not, so a tablet plugged in after connecting still
     negotiates pressure without a reconnect.
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
8. **Express keys/ring:** in the tablet vendor's control panel on the Mac,
   map express keys and the touch ring/strip to ordinary keys or a scroll/zoom shortcut, then confirm those
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

The virtual devices carry the session's display in their name, for example
`Arcen Virtual Tablet Pen (session :58)`; the session's Xorg admits only the
devices named for its own display (`xinput list` on the session display shows
no console keyboard or mouse).

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
   axes (confirming the two devices stayed separate). `evtest` reports
   `ABS_PRESSURE` with maximum `65535` (the shared vendor-neutral range;
   0.15 Piers declared `8191`).
2. Confirm no rotation axis is advertised (`libinput list-devices` should
   not show a rotation capability for this device) — this is the documented
   `Unavailable` state, not a bug.
3. Open **Krita** (or another approved Linux creative test application) in
   the same dedicated Xorg session and confirm a brush stroke shows varying
   width/opacity as physical pressure changes, and that tilt-sensitive
   brushes respond to physical tilt.
4. Confirm **no tablet vendor driver package** is installed or required on this
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
3. Flip to the eraser end; confirm the surface reports it inverted
   (`PEN_FLAG_INVERTED`) while it hovers, without a click, and adds the
   eraser flag only when it touches. Press either barrel button; confirm the
   surface reports the pen barrel (Windows Ink has one, reported as the
   second pointer button).
4. Confirm hover (in-range, not in contact) is visually distinct from
   contact (tip down), and that the injected coordinates land at the
   correct point on the session's selected display — including, in a
   multi-monitor session, a Deck display placed left of or above the
   primary.

   Without a physical pen, `arcen-deck input-smoke … --pen-full` sends a
   fixed script (hover row, tip stroke with a pressure ramp and tilt, barrel
   stroke, eraser hover and rub, second-button tap, corner taps) whose
   values a `WM_POINTER` logger in the session can check one by one.
5. Confirm **no Wintab-only application** is expected to receive input here
   — this backend is Windows Ink/`WM_POINTER` only, and Arcen does not
   support Wintab ([ADR 0017](../adr/0017-forward-only-pen-input.md)). Set
   each drawing application to Windows Ink as in
   [section 4a](#4a-windows-applications-choose-windows-ink).
6. Force `CreateSyntheticPointerDevice` to fail (e.g. run on an
   unsupported/older Windows build, or with Windows Ink disabled via Group
   Policy) and confirm the Pier advertises every pen capability as
   `Unavailable` in `ServerHello`, the attachment proceeds mouse-only, and
   no partial/fake pressure is reported.
7. Disconnect mid-contact; confirm proximity/contact is released (no stuck
   pointer state) and a subsequent reconnect creates a fresh, clean
   `PenInjector`.

## 4a. Windows applications: choose Windows Ink

> **Wintab is not supported.** Arcen gives a Windows host a Windows Ink pen
> and nothing else. Windows itself has no Wintab: `wintab32.dll` comes with a
> tablet maker's driver, and Arcen does not install one, ship its own, or
> pretend to be a tablet maker's device. An application set to Wintab sees no
> tablet over Arcen and draws with mouse pressure; switch it to Windows Ink.
> Native tablet (USB bridged), which would let the host's own tablet driver
> provide Wintab, is not offered on Windows hosts either. Why:
> [ADR 0017](../adr/0017-forward-only-pen-input.md).

A Windows Pier gives the session a Windows Ink pen
(`CreateSyntheticPointerDevice` / `InjectSyntheticPointerInput`, pressure
`0..=1024`, tilt, rotation, eraser, barrel button). Most drawing applications
offer both Wintab and Windows Ink; set them to Windows Ink (or pointer input,
or Real-Time Stylus — the same Windows path under other names):

| Application | Setting |
| --- | --- |
| Photoshop | Windows Ink is the default. If `PSUserConfig.txt` contains `UseSystemStylus 0`, remove that line (it selects Wintab) and restart. |
| Krita | Settings › Configure Krita › Tablet settings: **Windows 8+ Pointer Input**. |
| Blender | Preferences › Input › Tablet API: **Windows Ink**. |
| ZBrush | Preferences › Tablet › Tablet Driver API: **Stylus** (WinTab is ZBrush's default). |
| Toon Boom Harmony | Preferences › Advanced: clear **Use Qt Wintab Tablet Support**, then restart Harmony. |
| TVPaint Animation | Preferences › Drawing: choose **Ink** instead of Wintab. |
| Corel Painter | Preferences › Tablet: **RTS-compatible (Real-Time Stylus)**. |
| Rebelle | Preferences › Tablet: **Microsoft Ink** or **Windows Pointer Device**. |
| Clip Studio Paint | Preferences › Tablet: **Tablet PC**. |
| Maya | Preferences › Interface › Tablet API: **Windows Ink** (or Automatic). |
| Substance 3D Painter | Uses Windows Ink since 2019.3; nothing to change. Adobe documents Windows Ink as unsupported in virtual machines, so a Pier that is a VM may give it no pressure. |
| Houdini 21 | Uses Windows Ink unless `HOUDINI_ENABLE_WINTAB=1` is set in its environment; leave that unset. |
| Mari 4.7 and earlier | Wintab only: not supported, no pressure through Arcen. Mari 4.8 and later support Windows Ink. |

The settings come from each application's own documentation and were not all
tried with Arcen; check pressure with a light and a heavy stroke after
changing one. Pressure has 1,025 steps on Windows whatever the tablet offers.
An application with no Windows Ink mode gets no pressure on a Windows Pier;
no Wintab route is planned.

## 4b. macOS Pier: CoreGraphics tablet events

A macOS Pier posts the tablet events a Mac tablet driver posts: a
`kCGEventTabletProximity` event (pen or eraser, capability mask `0x25c7`) when
the tool arrives and leaves, and pointer events with the tablet-point subtype
carrying pressure in both `kCGMouseEventPressure` (what `-[NSEvent pressure]`
reads, 256 levels) and `kCGTabletEventPointPressure` (full precision), tilt,
rotation and barrel buttons (first barrel = right button, second = middle).
No entitlement and no driver on the host is needed for it.

1. Open a pressure-aware application on the host (Krita, Photoshop, or any
   AppKit canvas) in a session with Tablet support negotiated.
2. Draw a stroke with varying pressure; confirm the width or opacity follows
   it. Before 0.19.0 every stroke arrived at full pressure.
3. Tilt and rotate the pen where the brush uses them; flip to the eraser end
   and confirm the application switches to its eraser (it is told through
   `tabletProximity:`).
4. Press a barrel button while hovering; confirm a right click.
5. At the login window the pen is a plain pointer (tip and barrel buttons
   only), and the hello says so.

**Browsers on the host.** Use Chrome, not Safari, for a browser pen test such
as a stylus test page. Safari reports every pen on macOS as a mouse with
pressure 0.5 whenever a button is down, whichever tablet and whatever driver
posts the events: WebKit's macOS mouse events take no tablet subtype, pressure
or tilt from `NSEvent`, and `WebMouseEvent::pointerType()` returns `mouse` on
macOS (WebKit `Source/WebKit/Shared/WebMouseEvent.h` and
`Source/WebKit/Shared/mac/WebEventFactory.mm`, main `69595416`, read
2026-11-12). Chrome takes the pen type from the proximity event and pressure,
tilt and rotation from each tablet-point event; measured through a macOS Pier
in Tablet support on 2026-11-12 (Chrome 153): `pointerType` pen, pressure
0.047–0.984, tilt X 40 to −30, rotation 0–180, eraser `buttons` 32. Chrome
reports `tiltY` with the opposite sign to `-[NSEvent tilt]` (its view is not
flipped, `components/input/web_input_event_builders_mac.mm`), so a pen
reported tilted towards the user by AppKit reads negative there; the host
passes AppKit's value through unchanged, as a local tablet driver does.

Without a person at the host, the loopback end-to-end test drives the Pier's
real stream and input path:
`cargo test -p arcen-pier-macos --test end_to_end pen_samples_sent_by_a_client_are_injected_as_tablet_events -- --ignored`.
It injects real events at 40–44 % of the main display, so put a window you own
there first, or nothing, and never run it on a desktop someone is using.

`arcen-pier-macos probe-pen-hid` (a signed build with the virtual HID
entitlement, run from the Aqua session) records why a virtual HID digitizer is
not used: on macOS 26.7.2 it reports `pointer_max_tilt: 0.0` and
`pointer_max_rotation: 0.0` for a pen that sent 50° and 230°.

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

One tablet (an Intuos5 touch L, `056a:0317`) was captured with its vendor driver installed
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
matching combination on one occasion and against none of them after the vendor
driver was restarted. Restart the vendor driver or replug the tablet.

Since 0.16.0 the client matches by digitizer usage page and the descriptor's
own classification, not by vendor id, and the Linux host admits a device only
through `arcen_usb_bridge::RawHidAdmission` (pen-bearing interface of a device
that classifies as a pen tablet). The measurement above predates that change.

### Host half: blocked, and by design it always was

A `/dev/uhid` device created with Wacom's vendor ID binds **no driver at all**
on RHEL 9, so no input device appears and there is no pen. `wacom.ko` is a USB
driver and a uhid device is not a USB device.

This is not a defect to fix here. It answers the open empirical question of
input-only USB bridging: the design already separates the implemented typed
path ("Light") from the native USB bridge ("Hard"), and the experimental
raw-HID path is lab scaffolding that must not grow into the bridge. The full
measurement, the reference's `usb-vhci` architecture, the out-of-tree kernel
module cost, and the reopened client entitlement question are all recorded
there — that document is authoritative; do not re-derive it from this one.

Operationally, for anyone testing today: **raw-HID passthrough cannot deliver a
working tablet whose Linux driver binds only to USB devices (as the measured
one's does) on a Linux Pier**, so do not enable it expecting one. Use the
typed pen path, which is the supported configuration.

## 5b. Tilt reaches the Linux host but is not observed by applications

Reported 2026-08-12: pen tilt works on a Windows Pier and not on a Linux Pier.
Not investigated further; this records the measurement
so the client side does not get re-examined needlessly.

**Update 2026-10-19 (real-screen stylus tests, Firefox 140 and Chrome 155 on
Xorg/GNOME 40 and on the Wayland lab seat pen).** Tilt does reach browsers,
and two host-side faults distorted it; both are fixed:

- The tilt axes declared no resolution, so libinput read them as a Wacom pen's
  ±64° spread over `-90..=90`: a 45° tilt arrived as 32° (Chrome on Xorg, every
  Wayland client). The axes now declare `TILT_UNITS_PER_RADIAN`.
- On Xorg the pen also moved the cursor and clicked through the absolute mouse
  device, so applications received every sample a second time from a device
  without tilt or pressure (Chrome: 32 % of inked samples at pressure 0;
  Firefox: a `mouse` pointerdown and pointer-type flips inside strokes). The
  tablet device alone is the pen now.

Firefox on Xorg reports tilt larger than sent (45° as 64°). This is the
deliberate trade-off of the fix, not a remaining fault: xf86-input-libinput
exposes tilt as a ±64 valuator (it caps Xorg tilt at ±64° for every tablet),
GDK hands that to Firefox normalised to ±1, and Firefox multiplies by 90. A USB
Wacom shows the same; Chrome reports the degrees.

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

## 5c. Native tablet (USB bridged) with any tablet

Every release Deck carries Native tablet and its privileged helper (IOUSBHost
capture, [ADR 0019](../adr/0019-iousbhost-usb-backend.md)); it needs macOS 13
on the Deck, and on an Intel Deck it is untested on hardware. A Linux Pier offers it only when the administrator has enabled it
(`usb.native_tablet = "enabled"`, `install-arcen-pier --native-tablet`; see
[Native tablet in the administration guide](pier-administration.md#native-tablet-usbnative_tablet)),
and refuses with "Native tablet is not enabled on this host…" otherwise. A
macOS Pier offers it when signed with the virtual-HID entitlement: with
pressure, tilt, eraser and barrel buttons for a model the Pier reads itself
(the Wacom Intuos5 touch L; measured with an emulated Intuos5, pressure 0.059
to 1.000, a real tablet not yet measured), buttons and proximity only for
models Arcen does not know yet
([ADR 0022 §14.1](../adr/0022-macos-pier-native-tablet-hid-adapter.md)).
Windows Piers never offer it ([ADR 0017](../adr/0017-forward-only-pen-input.md)).
Keep the Deck's cursor rendering on **Local**: the Pier reports where the
host's pointer is and the Deck draws it there. When the session ends the
helper hands the tablet back to macOS like an unplug and replug. The privileged
`arcen-usb-helper` picks the first attached device whose interfaces are all HID
and whose report descriptors classify as a pen tablet. It reads them from the
I/O Registry (`IOHIDDevice` `ReportDescriptor`), so keyboards and mice it
examines are never opened or sent anything; only an interface the registry
left undescribed, on a device the registry already shows to be a digitizer
with no boot-keyboard interface, is read with `GET_DESCRIPTOR(Report)`. An
Intuos5-style tablet (pen on the vendor-defined Digitizers page `0xFF0D`)
qualifies; its axes are reported as unknown because its reports are opaque. It sends the descriptors to the
Deck, which forwards them in `client_hello.usb_hard_device`.

1. With any pen tablet attached, run `sudo arcen-usb-helper --probe` on the Mac
   and confirm `capture=ok` and the helper's
   `pen tablet <vid>:<pid> admitted: evidence=... pressure_levels=...` line.
   With no tablet attached it must report `no pen tablet is attached`.
2. A tablet whose express keys enumerate as a keyboard or keypad is bridged
   whole, express keys included. A device with a joystick, gamepad or
   multi-axis controller collection is refused with "not a tablet function
   and is never bridged; use Tablet support"; a device with no pen digitizer
   (a plain keyboard) is never a candidate at all.
3. Connect with Native tablet selected. On the Linux Pier, the
   `Hard USB negotiation inputs` log line shows
   `client_usb_hard_descriptors` (non-zero), `tablet_pressure_levels`, and
   `tablet_refusal` absent. A refused device names its reason in
   `tablet_mode_result.reason` (for example a 0.15 Deck: "the Deck supplied
   no HID report descriptors; Deck 0.16.0 or newer is required").
4. Confirm the host's own driver for that tablet binds the bridged device
   (`dmesg`, `libinput list-devices`). A device that serves a descriptor
   contradicting its authorization ends the session with
   `event=usb_hard_device_refused` and the reason in the Pier log.
5. **X11 (dedicated Xorg desktop):** the tablet works on the first
   connection ([ADR 0021](../adr/0021-bridged-usb-devices-on-their-own-seat.md)).
   It stays on `seat0`, tagged for this desktop's X server alone, which
   hot-adds it when it connects and drops it when the Deck disconnects. The
   Pier logs `Hard USB bridge up for this X11 desktop only` and `bridged
   tablet nodes verified`; the desktop's `Xorg.log` shows `config/udev:
   Adding input device Wacom …` and `Applying InputClass
   "arcen-bridged-usb-session"`. Only tablets the kernel `wacom` driver
   serves, with xf86-input-wacom installed, are taken; others are refused
   with a reason (Native tablet stops, the session goes on). Preconditions:
   `/etc/X11/xorg.conf.d/90-arcen-bridged-usb.conf` (installed by the
   installer) and every running X server started after it; otherwise the
   Deck is told "Native tablet is not ready on this host…" and the Pier logs
   the X server's pid. Restart the display manager once after installing
   (`systemctl restart gdm` while nobody is signed in at the console), or
   reboot. A desktop an earlier Pier started does not admit the tablet.
   The desktop's class turns xf86-input-wacom's `PressureRecalibration`
   off: over a network round trip a pen often arrives already touching the
   tablet, and the driver would otherwise take that contact as zero pressure
   and press no button (pages show pressure but draw nothing). `xinput
   list-props "<tablet> Pen stylus"` shows `Wacom Pressure Recalibration
   (…): 0`.
6. **Isolation checks:** `udevadm info /dev/input/eventN` of each bridged node
   shows `ID_INPUT.tags=arcen_bridge,arcen_bridge_<display>`,
   `LIBINPUT_IGNORE_DEVICE=1`, no `ID_SEAT` other than `seat0`, and neither
   a `uaccess` nor a `seat` tag; `getfacl` shows no user ACL; GDM's `/var/log/Xorg.0.log`
   shows only `Adding input device …` followed by `Ignoring device from
   InputClass "arcen-bridged-usb-ignore"` (no driver loaded, no `got fd`);
   `ls /run/udev/rules.d` shows one `72-arcen-usb-bridge-x11-<display>-….rules`
   per bridge. After the Deck disconnects, the port, the rule and the nodes
   are gone.
7. **Upgrade and rollback:** the rules are volatile. A starting Pier removes
   rules whose helper has ended, and `install-arcen-pier --uninstall` removes
   any that remain and the Xorg ignore file. After rolling back to a Pier
   without this support, remove leftovers with
   `rm /run/udev/rules.d/72-arcen-usb-bridge-*.rules && udevadm control
   --reload`; the ignore file only ever matches devices Arcen tagged.

### Linux host: the virtual host controller

The Linux Pier presents the tablet on the distribution's in-tree USB/IP
virtual host controller, `vhci-hcd` ([ADR 0020](../adr/0020-linux-native-tablet-in-tree-vhci-hcd.md)).
On Rocky Linux and RHEL it ships only in `kernel-modules-internal`, which the
installer installs for the running kernel when Native tablet is enabled (opt
out with `--no-native-tablet-modules`). The out-of-tree `usb-vhci`
(`/dev/usb-vhci`) is used only as a development fallback when `vhci-hcd`
cannot be loaded. An enabled Pier loads `vhci-hcd` once, when it starts; connections only check that
it is present. After installing `kernel-modules-internal` on a running host,
restart the Pier (or run `modprobe vhci-hcd`) before Native tablet is offered.

1. `arcen-pier validate-config` prints `Native tablet: off: …` while
   `usb.native_tablet` is not `"enabled"`; otherwise `Native tablet: vhci-hcd is loaded
   for kernel …`, says it is installed but needs a Pier restart (or
   `modprobe vhci-hcd`), or gives the reason it is missing with the exact
   command, for example
   `dnf --enablerepo=devel install kernel-modules-internal-6.12.0-211.66.1.el10_2.x86_64`.
   `modinfo -F signer vhci-hcd` names the distribution's kernel signing key.
2. While a Deck bridges a tablet: `dmesg` shows `vhci_hcd vhci_hcd.0: … Device
   attached` and the tablet's own driver (for example `wacom`) on
   `usb-vhci_hcd.0-<port>`; `/sys/devices/platform/vhci_hcd.0/status` shows the
   port in use; `/run/udev/rules.d/72-arcen-usb-bridge-*.rules` names the seat.
3. On a native-seat Wayland desktop the bridged nodes belong to the
   desktop's own seat, never `seat0`: `udevadm info /dev/input/eventN` shows
   `ID_SEAT=seat-arcen-pen-…` and no `uaccess` tag, and `loginctl
   seat-status seat0` does not list them (for X11 see step 6 above). On a
   native-seat Wayland desktop the desktop's `gnome-shell` holds the nodes
   (`ls -l /proc/<pid>/fd`), and `libinput debug-events --udev <seat>` shows
   `TABLET_TOOL_TIP` with pressure. A pen that lands already pressing (one
   bridged report per network round trip, about 41 ms) is still a full
   stroke there: libinput detects a pressure offset only on a proximity-in
   that hovers above half the distance range, and a pressing pen reports
   distance 0.
   With a Local cursor the Deck draws its pen dot where the pen is, on X11
   and Wayland alike, from the pen's own evdev reports (see
   [Native tablet: one path](#native-tablet-one-path)). The Pier log
   (`logging.level` 2) says `reporting the bridged pen's position from its
   own node` when the attachment starts and `bridged pen position reader
   ended` when the tablet goes; on X11 the cursor watcher also logs `the
   bridged pen's mapping on this desktop` (`WholeArea`, or the remap that
   hands the cursor back to the X pointer). A multi-output Wayland desktop
   reports no host positions yet.
4. After the Deck disconnects, the port is free again in `status`, the rule
   is gone, and the tablet's devices are gone.

## 5d. Native tablet with an emulated Wacom (no pen in hand)

A Deck built with the lab-only `usb-emulate-lab` feature replaces the
captured tablet with an emulated Wacom Intuos5 touch L (Linux names `0x0317`
"Intuos Pro L") that plays a scripted
pen stroke (`arcen-deck input-smoke … --usb-emulate wacom-intuos5`). The Pier
negotiates and presents it exactly like a captured Wacom, so the host's own
`wacom` driver binds it. `scripts/lab/wacom-replay/verify-pen.py`, run as root
on the host, records the pen's evdev events and asserts position, pressure,
proximity and the barrel button for every stroke; its README also gives the
X11-level check for the session. `--usb-emulate-stroke landing` plays strokes
whose first pen packet already presses (no hover first), as a bridged pen put
down between two polls arrives; `alternate` mixes both kinds; `realistic`
plays a stroke shaped like a hand on a bridged tablet (in-range packets, the
KP-501E Grip Pen's tool id, a hovering descent, a landing already pressing,
varying pressure and a held tilt); `tilted` plays the scripted stroke with
both tilt axes sweeping through both signs; `rest` and `hover-rest` end with
the pen still pressing or still hovering in proximity and hold it there for
eight seconds, so a lab can cut or end the session while the pen rests;
`upper-held` plays `realistic` with the upper side switch held throughout.
None of them is a recording of a real pen: an emulated proof never stands in
for the physical test. A lab Deck also takes these flags after `--connect
HOST PORT`, so the GUI plays the emulated pen and its host-led cursor can be
checked on screen. The emulated
smoke prints the `cursor_mode_result` and every host cursor position it
receives (`cursor-position …`). This proves the bridge and the host driver
without a person; it does not replace the physical test below.

## Native tablet: one path

Native tablet means one thing on every Pier: the tablet itself is attached
to the host, the host's driver (Linux) or the Pier's own pen reader (macOS)
interprets it, and the Deck shows where the pen is. Since 0.19.0 there is
one implementation of "where the pen is"
([ADR 0023](../adr/0023-one-native-tablet-pen-position-path.md)):

| Step | Linux Pier (X11 and Wayland) | macOS Pier |
| --- | --- | --- |
| Read the pen | the bridged pen's own evdev node, read-only, never grabbed, opened `O_NOFOLLOW` and proven to be the verified device | the Pier's Intuos pen reader, from the tablet's own packets |
| Shared sample | `arcen_input::tablet_pointer::PenSample` (device units, ranges, tool, time) | the same |
| Who leads | the pen while it is in proximity and the desktop maps the whole tablet to the whole desktop; otherwise the X pointer or GNOME's core pointer | the pen |
| To the Deck | `CursorPosition` the moment each report arrives (shared `HostPointerFeed`) | the same |

**Enabling it.** On Linux an administrator sets `usb.native_tablet` to
`"enabled"` in `/etc/arcen/pier.json` (or runs `install-arcen-pier
--native-tablet`) and restarts the Pier; nothing else enables it (the 0.18
`ARCEN_USB_HARD_LAB=1` service environment is no longer read). A macOS Pier
offers it when its signed build holds the virtual-HID entitlement. On the
Deck, choose **Native tablet (USB bridged)** for the connection.

**What you see.** While the pen is near the tablet, the Deck draws a small
white dot with a dark ring where the pen points, on every host, whatever
cursor the host application shows. Move the Mac's mouse and the Mac's own
cursor takes over at once. When the pen leaves the tablet the dot stays
where it left.

**Remapped tablets on X11.** The dot is computed for
`xf86-input-wacom`'s defaults: the whole tablet area, no rotation, an
identity "Coordinate Transformation Matrix", absolute mode. The Pier reads
those properties from the session's X server once a second; if any differs
(you mapped the tablet to part of the screen, rotated it, or switched it to
mouse mode) the dot follows the X pointer instead, a little later. On GNOME
Wayland the default mapping is assumed; reading a remap from GNOME Settings
is a follow-up.

**A held side switch looks like "pen but no ink".** If the upper side
switch of the pen is held (or reported held by a worn switch), GNOME on X11
treats it as a held right button: browsers see the tip as a chorded move,
pressure shows but nothing draws. Lift the pen out of range and put it back
(reseat), check the switch, or remap the upper switch in the desktop's tablet
settings. On a Mac, Wacom's preferences may map the same switch to
double-click.

**Safari on macOS reports every pen as a mouse.** WebKit on the Mac gives
pages `pointerType: "mouse"` and no pressure for any pen, local or remote.
Test pen pages in Chrome (or Firefox) on a macOS Pier.

### Lessons (do not forget)

1. **Emulated pens passed while the real tablet failed.** In November 2026
   every emulated Intuos5 proof passed on X11, Wayland and macOS while the
   real Wacom failed on all three. Every pen change needs a real-tablet or
   real-browser proof.
2. **A hung or held side switch looks exactly like "pen but no ink".** Read
   `BTN_STYLUS`/`BTN_STYLUS2` on the evdev node and the X button state before
   theorising about timing or grabs.
3. **Event-driven evdev positions beat X polling for perceived latency.**
   The X11 dot used to follow `XQueryPointer` read every 8 ms after
   `xf86-input-wacom` had processed the event and felt laggy, while the
   Wayland path, which already read the pen's own reports, did not. The
   numbers are in ADR 0023.

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
physically moving and pressing a real pen** against the attached
tablet, observed live through Deck's Tablet Monitor panel (section 1) and
confirmed to reach a real remote creative application on both Linux (section
3) and Windows (section 4). Do not report this feature as fully validated,
and do not claim a physical end-to-end run occurred, until that exact
physical interaction has actually happened and been recorded by whoever
performed it.
