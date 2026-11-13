# Emulated Wacom: prove a host's Native tablet path with no pen in hand

A Deck built with the lab-only `usb-emulate-lab` feature can bridge an
**emulated Wacom Intuos5 touch L** (`056a:0317`; the Linux `wacom` driver
names it "Intuos Pro L", so its pen node is "Wacom Intuos Pro L Pen") instead
of capturing a physical tablet:

```sh
cargo build --release -p arcen-deck-macos --features usb-emulate-lab
arcen-deck input-smoke <pier> <port> --pin-sha256 <fp> --credentials-stdin \
  --usb-emulate wacom-intuos5 [--usb-emulate-strokes 3] [--usb-emulate-lead-in-ms 3000]
```

`--usb-emulate` implies `--tablet-mode hard`. The Deck runs the shared
exporter (`arcen_usb_bridge::UsbExporter`) in-process over
`SyntheticDevice<WacomIntuos5>`: the Pier receives the same
`client_hello.usb_hard_device` and the same URB traffic as from a captured
Wacom, and answers it the same way (`usb-vhci` today). Control transfers are
answered from the descriptor fixture
(`shared/usb-bridge/fixtures/wacom-intuos5-touch-l/`), `SET_REPORT` is
stored and acknowledged, `GET_REPORT` echoes it (the `wacom` driver's mode
switch), and interrupt IN polls on the pen endpoint are answered with the
scripted stroke, one report per poll, once the host has switched the tablet
into Wacom mode and opened the pen. The smoke prints
`input-smoke complete usb_hard=true emulated=wacom-intuos5 strokes=N reports=M`.

Nothing in the emulation depends on the host's transport: the same
descriptors, control answers and reports serve a `uhid` presentation through
`WacomIntuos5::{report_descriptor, get_report, set_report}`.

## Kernel-level check (evdev) — `verify-pen.py`

On the Linux Pier host, as root, start this before the Deck connects:

```sh
python3 verify-pen.py --wait 240 --seconds 200 --idle 20 --min-strokes 1 \
  --log pen-events.log --json pen-summary.json
```

It waits for the `Wacom … Pen` input device of `056a:0317`, opens its event
node (which is what starts `usbhid` polling the pen), writes every event in
`evtest`'s format to `pen-events.log`, and checks each stroke: tool id
`0x802` and serial `0x0a2ce017`, hover at (16000, 12000) with distance 20,
`BTN_STYLUS` pressed and released while hovering, touch-down at the start
with pressure 127, 255, … 2047, the path through (48000, 12000) to
(48000, 28000) at pressure 1024, lift-off to pressure 0, proximity out. It
needs only Python's standard library (no `evtest` or `libinput-utils`).

## Display-server check (X11) — after the seat fix

The kernel-level check proves the device and its events reach the host. Whether
the **session's** display server uses them is a separate question: until
bridged devices are assigned to the session's own seat, a session Xorg that
filters devices by name ignores the bridged tablet, and the console's GDM
Xorg may take it instead. Once the seat fix is in, check it in the session:

```sh
# as root on the host, with the session's Xorg display and -auth file
DISPLAY=:<n> XAUTHORITY=<auth> xinput list            # lists "Wacom … Pen stylus"
DISPLAY=:<n> XAUTHORITY=<auth> xinput test-xi2 --root # while the Deck replays
```

Expect motion events at the scripted positions (scaled to the screen),
`ButtonPress 1` on touch-down, `ButtonPress 2` for the barrel button, and
pressure on the device's third valuator. A paint application in the session
(for example GIMP or Krita with the pen's pressure curve) shows the stroke as
an L, thicker during the ramp.

## Descriptors

`capture-descriptors.py` (macOS) reads an attached Intuos5's descriptors
without capturing it and compares them with the fixture. `--write` saves the
real bytes to a separate, deliberately unlicensed `…-captured/` directory
(`reuse lint` fails until Release/Security annotates it); see the fixture's
README.

The release packaging refuses this feature: `build-deck-app.sh --release`
rejects `ARCEN_DECK_FEATURES` containing `usb-emulate-lab` (or `dev-tools`,
`experimental-raw-hid`) and any binary carrying the
`ARCEN_DECK_USB_EMULATE_FEATURE_MARKER_DO_NOT_SHIP` string.
