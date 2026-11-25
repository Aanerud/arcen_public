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

## A GeForce with a monitor or a dummy plug

From the release after 0.20.5 the Pier also serves a GeForce's own display:
a monitor, or an HDMI or DisplayPort dummy plug, which Windows treats as a
monitor with the plug's own EDID. It reads the card's class before it
changes anything and never asks a GeForce to write an EDID. It sets the mode
of that display nearest to the Deck's size, from the modes the display
offers, through Windows' own display settings. When the Deck's size is not
one of them, the Deck says so, for example *Host could not set 1800x1168; it
is serving 1920x1080* and *exact display needs a Quadro, RTX Pro or GRID GPU;
this host has a GeForce RTX 5090, so it serves the modes its attached display
offers*. A GeForce with no display at all is refused with a message that asks
for a monitor, a dummy plug or a virtual display driver.

The Pier does not change the Windows display scale of that display: the desk
keeps its own, and when it is not the Deck's Remote UI scale the Deck shows
the scale Windows uses, so it can be set in Windows Display settings. An HDR
session first sets the display's own mode nearest to the Deck's size, then
turns HDR on for that display, and is served as HDR only when Windows shows
HDR there. The Deck is told the refresh the display runs at.

Dummy plugs work. Desk privacy (`privacy.desk`, `on` by default) prefers a
display no monitor at the desk shows, but Windows reports a dummy plug as a
physically connected monitor, so the Pier cannot prove the plug dark. With
`on` it keeps the session, blocks the desk's keyboard and mouse, and tells
the Deck: *Host screens could not be blanked (a dummy plug or a monitor that
cannot be darkened). People at the host may see this session.* Nothing more
is needed where nobody can see the plug's output. To keep the screens
private as well, install the virtual display driver above: the Pier serves
the virtual display and detaches the plug while a Deck is attached. Only
`"privacy": {"desk": "required"}`, for sites whose rules demand it, refuses
such a PC (`console_privacy_unavailable: desk_monitor_lit`).

### Applications that draw with OpenGL or CUDA on the display

An application that draws its own interface on the GPU through OpenGL and
CUDA, such as DaVinci Resolve, needs its window on a display the NVIDIA GPU
drives itself. On a virtual (indirect) display, as in a Remote Desktop
session, the window may not get the NVIDIA OpenGL driver, and Resolve's
interface stays grey or black there while its video output card and
control panel keep working (reported on a GeForce RTX 5090 Pier with no
monitor attached; Blackmagic's forum reports the same under Remote
Desktop). On such a
PC keep a monitor or a dummy plug on the GeForce and serve the session on
it. With desk privacy `on` the session goes on and the Deck is told that
the host's screens are visible; any monitor on the GeForce (in a machine
room, for example) shows the session, while the desk's keyboard and mouse
stay blocked.

Pinned deskside protection (`platform.desktop.deskside.enabled`) requires
the Deck's exact display, which a GeForce cannot provide: such a host is
refused before anything changes, with a message naming the card.

### Earlier releases (0.20.4 and 0.20.5)

These releases asked a GeForce's monitor or dummy plug to take an EDID.
NVIDIA refused (`NvAPI_GPU_SetEDID returned NVAPI status -104
(NVAPI_NOT_SUPPORTED)`), the session failed with *exact display apply
failed … rollback also failed*, and the display recovery journal it left
could not be restored. Nothing was written to the display: NVIDIA refused
the write. On those releases the next session sets the journal aside as
`display-recovery.unrestorable.<time>.json` and fails the same way, unless
the session is served on a display NVIDIA does not drive, the virtual
display driver above (keep desk privacy on so the Pier chooses it). After
upgrading:

1. Connect once from the Deck. A journal still pending from such a session
   is restored on its own; the session log says `this GPU refuses EDID
   writes, so the attempted write changed nothing`.
2. Leave the files
   `C:\ProgramData\Arcen\runtime\display-recovery.unrestorable.*.json` as
   they are. They are records of the failed sessions, set aside so that they
   block nothing.

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
