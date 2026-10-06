# Windows Pier on a GeForce PC with a virtual display

**Status: streaming on one host.** A macOS Deck has streamed and controlled
the desktop of one PC in this configuration: an NVIDIA GeForce RTX 4080,
Windows 11, no physical monitor, and the third-party *Virtual Display Driver*
below. Capture used Desktop Duplication and NVENC encoded AV1 at about 2–3 ms
a frame. It is not yet tested on other GeForce models or driver versions.

A GeForce card gives the Pier NVENC hardware encoding, but not everything a
professional NVIDIA card does. In particular, NVIDIA lets software write a
display's EDID only on Quadro, RTX Pro and GRID GPUs, which is how the Pier
creates a display of exactly the Deck's size on those cards. On a GeForce
with no monitor attached there is no display to capture, so a virtual display
driver has to provide one.

## The driver being tested

[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver),
release [25.7.23](https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/tag/25.7.23),
installed with its *Virtual Driver Control* app. It is an Indirect Display
(IddCx) driver: Windows renders its display on the NVIDIA GPU, so the Pier
captures and encodes it on the same GPU. Arcen does not ship, sign or support
this driver; it is a separate project with its own licence.

## Setting it up

1. Install the driver and create one virtual display with Virtual Driver
   Control.
2. Add the resolution your Deck asks for to the driver's mode list,
   `C:\VirtualDisplayDriver\vdd_settings.xml`. The Pier cannot create a mode
   on a GeForce; it picks the closest mode the driver offers. The driver's
   default modes are all 16:9, so a 16:10 Mac screen gets 1920×1080 with bars
   (and 800×600 when no other mode can be set). The size the Deck asks for is
   in the Deck log as `reporting client display layout to host`, for example
   `1800x1168` for a fullscreen 14-inch MacBook Pro.
3. Make sure nothing raises a UAC prompt at sign-in. On a PC with no monitor
   the prompt is shown to nobody, Windows refuses screen capture while it is
   up, and the Deck sees black. Find the program that asks and stop it from
   starting at sign-in; lowering UAC to *Never notify* also works, but it
   removes a Windows security boundary.
4. Install the Windows Pier as usual.

## The pointer

The Deck draws your own pointer. If you see a second one trailing it, the
virtual display has Windows draw the pointer into the picture. Make sure
`<HardwareCursor>true</HardwareCursor>` is set in `vdd_settings.xml` and reload
the driver. With logging at level 3 the session log's
`capture and encode loop detail` lines show `capture_pointer_updates`: above
zero while you move the mouse means the pointer is kept out of the picture.

## Responsiveness

Auto streams at 30 frames a second. Choose **Speed** on the Deck for 60, and
give the virtual display a 60 Hz mode in `vdd_settings.xml`.

## What the Pier does on this host

- It sees that NVIDIA does not drive the virtual display, or that the card is a
  GeForce, and serves the existing display without writing any EDID. The
  session log says `serving the existing display at the nearest mode instead`.
- It sets the closest mode the driver offers to the Deck's size.
- If Windows refuses screen capture because the secure desktop is showing (a
  lock screen, the sign-in screen or a UAC prompt), it waits up to five seconds
  for it to close, then names the desktop in the session log:
  `DXGI Desktop Duplication refused; input desktop: …`.

## If the picture is black

Set `logging.level` to `3` in `C:\ProgramData\Arcen\pier.json`, run
`sc.exe control ArcenPier 201`, connect once from the Deck, then collect a
support bundle with `arcen-pier.exe support-bundle` from an elevated
PowerShell. In the newest `logs\sessions\arcen-session-agent-*.log`:

| Line | Meaning |
| --- | --- |
| `input desktop: not accessible from this session` | The secure desktop is showing: a lock screen, the sign-in screen or a UAC prompt. Find and close it. |
| `input desktop: Default` | The desktop is accessible and capture was refused for another reason. Report it with the bundle. |
| `no desktop frame after 1s` repeating | Capture started but Windows delivered no frame of that display. Report it with the bundle. |
