# Arcen Deck user guide

This guide is for the people who work in a remote desktop all day: artists,
editors, colourists. It follows the order you meet things, from installing
the app to signing out. Administrators who run a host (a **Pier**) want the
[administration guide](operations/pier-administration.md) instead.

A few words first:

- **Deck** is the app on your Mac.
- **Pier** is the program on the workstation you connect to. It can be a
  Linux, Windows or Mac workstation. The Mac host is a preview.
- Your **administrator** is whoever set up that workstation. Several answers
  below end with "ask your administrator", because the host decides some
  things and the Deck cannot override them.

Every section carries a hidden note naming the source files it was checked
against, so it can be checked again when the app changes.

**Contents**

1. [Install the Deck](#1-install-the-deck)
2. [Connect and sign in](#2-connect-and-sign-in)
3. [Choose a preset](#3-choose-a-preset)
4. [Check your colour](#4-check-your-colour)
5. [Pen and tablet](#5-pen-and-tablet)
6. [Keyboard](#6-keyboard)
7. [Clipboard](#7-clipboard)
8. [Sound and microphone](#8-sound-and-microphone)
9. [Several displays and Retina](#9-several-displays-and-retina)
10. [How the connection is doing](#10-how-the-connection-is-doing)
11. [Reconnecting after a dropout](#11-reconnecting-after-a-dropout)
12. [Disconnecting, quitting and signing out](#12-disconnecting-quitting-and-signing-out)
13. [When something goes wrong](#13-when-something-goes-wrong)
14. [Where to get help](#14-where-to-get-help)

---

## 1. Install the Deck

<!-- source: README.md (The client (Deck), Uninstall, What works today); clients/macos/src/usb_helper_install.rs -->

You need a Mac with macOS 12 or later.

1. Download the app that matches your Mac:
   - `Arcen-Deck-<version>-macOS.zip` for Apple silicon (M1 and later);
   - `Arcen-Deck-Intel-<version>-macOS.zip` for an Intel Mac.
2. Unzip it and drag **Arcen Deck** (or **Arcen Deck Intel**) to
   `/Applications`. Both apps are signed and notarised by Apple, so they open
   normally.
3. Install only one of the two. They share settings and privacy permissions
   and are not meant to sit side by side.

Good to know:

- AV1 video needs Apple silicon M3 or later. An Intel Mac never asks for it.
- **Native tablet**, an optional pen mode (see [Pen and tablet](#5-pen-and-tablet)),
  needs macOS 13 or later.
- If macOS asks whether Arcen Deck may find devices on your local network,
  allow it. Without it the Deck cannot reach a workstation on your network.

To remove the Deck later, quit it and drag it to the Bin. Your saved
connections, remembered host certificates and settings stay in
`~/Library/Application Support/Arcen` until you delete that folder.

## 2. Connect and sign in

<!-- source: clients/macos/src/ui/home.rs (Connections sidebar, New Connection form, Remember username, Auto-reconnect, Test connection); clients/macos/src/ui/app.rs (cert_warning_modal, certificate_changed_modal, Security modes, Review before connect, connection_serialization_never_contains_passwords); clients/macos/src/ui/connection_test.rs; README.md (Connect) -->

### Save a connection

Your administrator gives you an address (a name such as
`workstation.example.internal`, or an address such as `203.0.113.10`) and
tells you which account to use. Arcen has no accounts of its own: you sign
in with your ordinary account on that workstation.

1. Open Arcen Deck and choose **+ Add Connection** at the bottom of the
   **Connections** list.
2. Fill in **Name** (anything you like), **Host Address**, and **Port**
   (leave it at 18444 unless told otherwise). **Remembered Username** is
   optional.
3. Choose **Add Connection**.

The same form has **Displays** and **Keyboard** choices for this connection
only. Leave them on their defaults to start with; sections 6 and 9 explain
them. To change a saved connection later, select it and choose **Edit**.

### Test before you sign in

Select a connection (or open its form) and choose **Test connection**. It
checks the whole way to the
workstation: the name, the network port, the host's certificate, the host's
greeting, the packet size and the round trip. It sends no user name or
password. Each failing step says **What to do**. **Copy report** puts the
result on the clipboard, ready to send to your administrator.

### Sign in

Select the connection, type your **Username** and **Password**, and choose
**Connect**. Tick **Remember username** if you like. The Deck never saves
your password.

If the host shows a notice before sign-in, the Deck shows it under **Review
before connect**. Choose **Accept** to continue or **Decline** to stop.

### The first time: verify this host

The first time you connect, the Deck shows **Verify this host** with the
host's fingerprint (**SPKI SHA-256**). Compare it with the one your
administrator gives you, then choose:

- **Trust and Remember**: the Deck remembers this host and warns you if its
  identity ever changes. This is the usual choice.
- **Trust Once**: trust it for this session only.
- **Cancel**: do not connect.

### If the host's identity changed

If a remembered host presents a different certificate, the Deck stops and
shows **This host's identity changed**. It has not sent your password. This
happens when a host is reinstalled or its certificate is renewed, and it is
also what an attack looks like. Ask your administrator before you continue.
Only when they confirm the change, tick **I confirmed with the host
administrator that its certificate changed** and choose **Forget and
Verify**. The Deck then shows the new fingerprint for you to compare, as on
the first connection.

### Certificate checking

**Settings › Advanced › Security modes** decides how strict the Deck is.
**Medium** is the default: a certificate that no trusted authority issued
gets the **Verify this host** prompt above. **High** refuses to connect
unless the certificate is fully verified. **Low** skips the check; avoid it.
Open Settings with the gear button (⚙) at the top of the Deck window when
you are not in a session.

## 3. Choose a preset

<!-- source: clients/macos/src/ui/app.rs (StreamingPreset, Settings › Streaming, NO_HDR_SCREEN, without_unavailable_hdr, paint_negotiated_truth_badge); shared/media/src/video/pipeline/{auto,speed,grading,hdr}.rs (summary strings); clients/macos/src/ui/session_truth.rs (PIPELINE DEGRADED); README.md (Streaming presets and independent pipelines); docs/operations/troubleshooting.md (Wrong or washed-out colours) -->

**Settings › Streaming** offers four presets. Each one is a complete
promise about frame rate and colour, served by its own path through the
host, not a label on the same picture.

| Preset | What it promises | Use it for |
| --- | --- | --- |
| **Auto** (default) | A sharp picture, up to 30 frames a second, 8-bit 4:2:0. When the network is tight it gives up frame rate, not sharpness. | Everyday work: email, browsing, most editing. |
| **Speed** | Smooth motion, up to 60 frames a second, 8-bit 4:2:0, with the lowest-latency encoder settings. When the network is tight it gives up sharpness. | Playback, scrubbing, animation, anything where motion matters more than fine detail. |
| **Grading** | 10-bit 4:4:4 BT.709 **SDR**, up to 30 frames a second. It trades some interaction latency for detail. | Colour-critical SDR work. Also the cure for coloured fringes on fine text. |
| **HDR** | HDR10: PQ transfer, BT.2020 primaries, 10-bit 4:4:4, up to 30 frames a second. It falls back visibly when the host cannot prove HDR. | HDR grading on a Mac display that can show HDR. |

Things worth knowing:

- **10-bit is not HDR.** Grading is ten-bit SDR. Only HDR uses the PQ
  transfer and your Mac's extended brightness (EDR).
- **HDR is greyed out** when none of your Mac's displays can show HDR
  ("No HDR-capable screen found"). If HDR was chosen and no display can
  show it, the Deck changes the choice to Grading before it connects and
  says so.
- A preset change applies to the next connection. If your session is
  running, the [Colour check](#4-check-your-colour) can switch preset and
  reconnect in one click.
- If it feels slow, try **Speed**. Asking for 60 frames a second makes the
  picture feel much faster, at a cost in bandwidth.

### When the host serves less than you asked for

A host serves only what it can actually deliver. When it serves less, it
says so, and the Deck shows a warning at the top of the picture that you
cannot switch off, for example:

> PIPELINE DEGRADED: requested grading, host served auto (eight_bit_source) …

followed by whether encoding and decoding run in hardware. Red means the
colour itself is not what you asked for; amber means something else
changed, such as the pipeline or the codec. The part in brackets is the host's reason, and it is often
followed by what you can do. Common cases:

- **HDR served as Grading on a Linux host using X11.** Expected: X11 cannot
  report HDR. You are still getting real ten-bit SDR.
- **Grading served as Auto on a Linux host** (`eight_bit_source`). The
  desktop was started for Auto or Speed and keeps that depth until you sign
  out of it. Sign out from the remote desktop, then connect again with
  Grading.
- **A host without a suitable graphics card** serves Auto and Speed in
  software and refuses Grading and HDR, with the reason.

If the warning says the Deck fell back to eight-bit presentation, reconnect.
If it stays, choose Auto and send the Deck log to your administrator.

## 4. Check your colour

<!-- source: docs/operations/colour-check.md; clients/macos/src/ui/colour_check.rs; clients/macos/src/ui/macos_menu.rs (Session › Colour Check…); clients/macos/src/ui/app.rs (Settings › Streaming › Colour check…) -->

Before you judge a picture, open **Session › Colour Check…** (or
**Settings › Streaming › Colour check…** at any time). It is a checklist
that answers one question: is what I see what the host has?

It marks each item ✓ (right for colour-critical work), ℹ (worth knowing),
⚠ (usable, with a caveat) or ✗ (what you see is not what the host has):

- **What the host serves**: preset, codec, chroma, bit depth, transfer and
  primaries, range and matrix, each compared with what you asked for, with
  the host's reason for any difference.
- **What your Mac does**: whether the display is in a reference mode, its
  colour profile, HDR headroom, the ten-bit presentation layer, EDR, and
  whether the picture is shown 1:1 without scaling.
- **Exactness, on demand**: choose a pattern, **Save pattern…**, show the
  saved PNG full-screen on the host at 100 %, then press **Measure**. The
  Deck compares every pixel with the pattern. A compressed stream is
  measured as close, not claimed bit-exact. Measure in Grading; HDR is not
  compared code for code.

Where the Deck can fix something itself, the item offers it: **Switch to
Grading** or **HDR** and reconnect (you sign in again; your remote
applications stay open), **Stream at Retina resolution**, or **Enter
fullscreen**. The Deck never changes a host setting. When the host needs a
change, the check names it and links the page your administrator needs.

**Copy report** and **Save report** produce a dated text report (saved to
`~/Documents/Arcen/Colour Checks/`) to attach to a review or a support
request. More detail: the [colour check guide](operations/colour-check.md).

For the best match, use a reference preset on your Mac's display (System
Settings › Displays › Preset) or a calibrated display, and turn off True
Tone and automatic brightness.

## 5. Pen and tablet

<!-- source: clients/macos/src/ui/app.rs (tablet_mode_label, tablet_mode_description, Settings › Input › Tablet, set_tablet_mode_requested); clients/macos/src/ui/macos_menu.rs (View › Tablet Monitor, Tablet Orientation Left-handed); shared/input/src/negotiation.rs and hosts/windows/src/session.rs (refusal texts); docs/adr/0017-forward-only-pen-input.md; docs/adr/0018-unified-typed-devices.md; docs/operations/pen-tablet-input.md; README.md (Pen and tablet) -->

Any pen tablet works, whoever made it. Choose the mode in **Settings ›
Input › Tablet**. The choice is saved with the connection you have
selected (the page names it), and takes effect on the next connection.
Every other saved connection keeps its own mode, so choose Native tablet
again for each host you want it on.

### Tablet support (the default, and the right choice almost always)

Your Mac's own tablet driver reads the pen, and Arcen sends finished pen
events to the host. Nothing waits for a reply, so it works over Wi-Fi, 5G
or a VPN, and the host needs no tablet driver. Distance costs you
responsiveness, not correctness.

- Pressure, tilt, rotation, eraser, proximity and the barrel buttons work.
- Finger touch and the tablet's own buttons stay on your Mac, and the tablet
  keeps working in Mac applications while you are connected.
- To use the tablet's express keys or touch ring in the remote desktop, map
  them to ordinary keys or scrolling in the tablet maker's control panel on
  your Mac. They then arrive as keys.

What the host makes of the pen:

| Host | What applications see |
| --- | --- |
| Windows | A **Windows Ink** pen, with 1,025 pressure steps. Set each drawing application to Windows Ink (or "pointer input", "Real-Time Stylus"). **Wintab is not supported**: an application set to Wintab gets no pressure. The [per-application settings](operations/pen-tablet-input.md#4a-windows-applications-choose-windows-ink) list where to switch. |
| Linux using X11 | A virtual tablet with pressure, tilt, eraser and barrel buttons. |
| Linux using Wayland | A real pen with pressure and tilt when the desktop runs on its own seat; on other Wayland desktops the pen acts as a mouse for now. |
| Mac (preview) | The tablet events a Mac tablet driver posts: proximity, pressure, tilt, rotation, eraser, barrel buttons. At the Mac sign-in window the pen is a plain pointer. |

**View › Tablet Monitor** shows what the Deck sees from your pen and whether
the host accepted it as a pen. **View › Tablet Orientation Left-handed**
mirrors the tablet for left-handed use.

### Native tablet (USB bridged): LAN only, where the host offers it

A small privileged helper takes the tablet away from macOS and forwards its
raw USB traffic, so the host's own tablet driver claims it as if it were
plugged in there. The whole device works, finger touch and buttons
included. In exchange, every pen sample makes a round trip to the host, so
it is for an office network, not a long distance; and Mac applications
cannot use the tablet until you disconnect.

Where it is offered:

- **Linux hosts, when their administrator has enabled it** (it is off until
  then). Your tablet joins an X11 desktop on the first connection, with no
  sign-out, and a Wayland desktop that runs on its own seat. A Wayland
  desktop without its own seat refuses it. On GNOME Wayland, lines come out
  thinner because GNOME squares pen pressure: raise the stylus pressure
  curve in the host's Settings › Wacom Tablet. If you remap the tablet
  there, the Deck's cursor no longer sits on the pen; keep the default
  mapping. A Wayland desktop on several displays does not move the Deck's
  cursor with the pen. GNOME hides its own pointer while the pen is near
  the tablet; the Deck then shows a small round mark where the pen is.
- **Mac hosts**: with pressure for a tablet model the host knows (the Wacom
  Intuos5 touch L today); other models get buttons and proximity only.
- **Windows hosts do not offer it**, by decision: it would need a signed
  kernel driver the project does not fund ([ADR 0017](adr/0017-forward-only-pen-input.md)).
- A host that cannot take it says why ("Native tablet is not enabled on this
  host…", "…is not offered on Windows hosts…", "…is unavailable on this
  host…"), and you keep ordinary mouse control. Choose **Tablet support**
  instead, or ask the host's administrator.

When you disconnect, the helper hands the tablet back to the Mac as if you
had unplugged it and plugged it in again, so Mac applications and the
tablet maker's driver get it back.

If you do use it, it needs macOS 13 or later and the Arcen USB helper (on
Intel Macs it has not been tried with a real tablet yet).
**Settings › Input › Tablet** installs the helper, and you approve it once in
System Settings › General › Login Items. The Deck itself never runs with
extra privileges. Keep cursor rendering on **Local** (the connection dialog recommends it):
the host's own driver
moves the host's pointer, and the Deck draws the pen's pointer where the host
reports it, without waiting for the video. Moving the Mac's mouse brings its
own pointer back at once. A host that cannot report its pointer (older
versions) leaves the bridged pen without a visible pointer; choose **Use host
cursor** there.

The project's direction is to describe each device by what it can do, so
the host builds a matching virtual pen (or other device) without a driver.
That design is [ADR 0018](adr/0018-unified-typed-devices.md); it is not
built yet, and nothing about Tablet support changes because of it.

### Mouse compatibility only

No pen redirection: the pen moves the pointer like a mouse, with no
pressure or tilt.

## 6. Keyboard

<!-- source: clients/macos/src/ui/home.rs (keyboard_field, shortcut_label, shortcut_description, keyboard_mode_label); clients/macos/src/ui/app.rs (Settings › Input › Typing, Paste as Keystrokes, request_quit, QUIT_HOLD); clients/macos/src/ui/macos_menu.rs (Connection menu items and shortcuts); clients/macos/src/ui/text_input.rs (Keyboard: Characters badge); clients/macos/src/ui/paste_keys.rs; shared/input/src/text/paste.rs (rates and limits); shared/input/src/text/policy.rs (defaults); README.md (Your Mac keyboard on the host) -->

### Your Mac keys on the host

The Deck learns what kind of host it reached and maps your keys the way
that host expects:

| Host | ⌘ Command | Control | ⌥ Option |
| --- | --- | --- | --- |
| Mac | ⌘ | Control | Option |
| Linux | Ctrl | Ctrl | Alt |
| Windows | the Windows key, except ⌘C, ⌘X, ⌘V, ⌘A, ⌘Z, ⌘F and ⌘W, which are Ctrl+C, X, V, A, Z, F, W | Ctrl | left: Alt; right: AltGr |

On a Windows host this follows Microsoft's Windows App for Mac: ⌘ on its
own opens Start, ⌘E opens File Explorer, and right ⌥ is AltGr, so on a
Norwegian Windows layout right ⌥2 types `@`. Keys are sent by position, so
choose the Windows keyboard layout that matches your Mac keyboard.

Each saved connection can change this (**Edit** › **Keyboard shortcuts**):

- **Automatic (by host)**, the default, as in the table.
- **⌘ is the Windows key, Mac edit shortcuts**: the Windows App behaviour.
- **⌘ and Control swapped**: ⌘ is Ctrl for every key, Control is the
  Windows key.
- **⌘ is Control**: ⌘ and Control are both Ctrl.

A Mac host always keeps ⌘ as ⌘. Tick **Left Option is also AltGr (Windows
hosts)** to type AltGr characters with left ⌥ too.

### Characters or keys

Normally the host's keyboard layout decides each character, which is right
for shortcuts and games. When the host's layout differs from your Mac's
(say, a US layout on the host under a Norwegian Mac keyboard), switch the
session to characters:

| Connection menu | Shortcut | What reaches the host |
| --- | --- | --- |
| **Keyboard: Physical Keys** | ⌃⌘K | Keys by position, as the Typing setting says. |
| **Keyboard: Characters** | ⌃⌘U | Every character as your Mac typed it (`æøå`, accents, ⌥ characters, input methods), whatever the host's layout. |

In Characters mode the picture shows a small **Keyboard: Characters**
badge. Shortcuts, arrows, function keys, Esc, Return, Tab and Backspace stay
keys. A game or a virtual-machine console that reads key positions may
misbehave; switch back with ⌃⌘K. A saved connection can start in either
mode (**Edit** › **Keyboard mode**). Characters mode needs a Windows or
Linux host from Arcen 0.18.0; a host that cannot type text says so and
keeps physical keys.

**Settings › Input › Typing** sets the everyday behaviour: **Characters as
on this Mac** (recommended: keys as keys, except characters the host's
layout would type differently and whatever your input method composes),
**Keys only**, or **Always characters**. The same panel sets what the left
and right ⌥ keys do.

### Paste as Keystrokes

Clipboard sharing does not reach a sign-in screen, a lock screen, a
Windows elevation prompt or a virtual-machine console. **Connection › Paste
as Keystrokes** (⌃⌥⌘V) types your Mac clipboard's plain text there instead.

- It waits until you let go of every modifier key ("Release the modifier
  keys to start typing"), so no character becomes a shortcut.
- It types 25 characters a second by default. Slow it down in **Settings ›
  Input › Paste as Keystrokes › Typing speed** for consoles that drop keys.
- It asks first above 2,000 characters (adjustable) and never types more
  than 100,000.
- **Esc** or **Connection › Stop Typing** stops it. Keys you press while it
  types are not sent.
- A line break at the very end is not typed, so a pasted password never
  submits a form on its own. Press Return yourself.
- The text is never logged. A host whose clipboard policy keeps your Mac's
  clipboard out keeps it out of Paste as Keystrokes too.

It needs a host that types text (Windows or Linux from Arcen 0.18.0). The
Mac host does not type text yet and says so.

### Two keys to remember

- **⌘Q in a session goes to the host.** Tap it and the host application
  gets it. Hold it for three seconds to quit the Deck. Choosing **Quit Arcen
  Deck** from the menu quits at once.
- **Connection › Release Modifiers** lets go of any modifier key the host
  still thinks is held, if keys start acting strangely after switching apps.

## 7. Clipboard

<!-- source: clients/macos/src/ui/app.rs (Settings › Clipboard, Effective host policy); docs/security/clipboard-redirection.md; docs/operations/troubleshooting.md (Clipboard does not work); README.md (What a session carries) -->

Copy on one side, paste on the other. Text and images work in both
directions, as far as the host allows: the host decides the direction, the
content and the size limit, and the Deck cannot override it.

- **Settings › Clipboard** shows **Effective host policy** for the current
  session, and has **Enable clipboard redirection** (on by default) if you
  want it off.
- Plain text and images are carried. Files and formatted text (HTML, RTF)
  are not.
- A very large copy can be refused by the host (8 MiB by default). The
  session carries on.
- What you pasted can stay on the other side's clipboard after you
  disconnect. Overwrite it if it was sensitive.
- For sign-in and lock screens, use [Paste as Keystrokes](#paste-as-keystrokes).

## 8. Sound and microphone

<!-- source: README.md (Audio out, Microphone in); docs/operations/config-reference.md (audio.local_playback); packaging/linux/arcen-pier.json; clients/macos/src/ui/app.rs (Settings › Microphone, ClientSettings default microphone_enabled false); docs/operations/audio-input.md; docs/operations/troubleshooting.md (No sound, or no microphone) -->

### Sound from the host

The host's sound plays on your Mac, in stereo, when the host has sound
turned on (the installed default).

On a **Linux host the host's own speakers are muted by default** for your
whole session, so nobody next to the machine hears your work. If the host
cannot prove its speakers are silent, it refuses the session ("The host
could not mute its own speakers…") rather than play aloud. Your
administrator can choose to let them play. A Mac host does the same; it
needs the System Audio Recording permission on the host for that, and
refuses the session until it has it. A Windows host does not mute its
speakers yet.

### Your microphone into the host

The microphone is off until you turn it on, every time you start the Deck:

1. Open **Settings › Microphone** and tick **Allow microphone forwarding
   for this launch**.
2. Allow Arcen Deck to use the microphone when macOS asks.
3. Connect. The host must allow it too.

Turning it off stops capture and needs a reconnect. A Linux host takes the
microphone. A Windows host cannot yet: it needs a signed driver the project
does not ship. On the Mac host (preview) it is built but not yet qualified.

## 9. Several displays and Retina

<!-- source: clients/macos/src/ui/app.rs (DisplaysMode labels and descriptions, Settings › Displays, Notch area, Remote UI scale, HiDPI / Retina streaming, Require 4:4:4, Full resolution, "This desktop was started without your layout"); clients/macos/src/ui/home.rs (displays_field); clients/macos/src/ui/macos_menu.rs (Show Fullscreen ⌃⌘F); clients/macos/src/transport/websocket.rs (MultiMonitorUnsupported); docs/operations/pier-administration.md (Display modes) -->

### How many displays

**Settings › Displays** has three choices:

- **Match my layout** (default): each of your displays, up to four, gets
  its own display on the host, shown full-screen and pixel for pixel. It
  needs macOS's "Displays have separate Spaces" turned on, **Extend
  fullscreen under the notch** turned off, and a host that supports it. If
  it cannot be done, the connection fails with a message that says why
  rather than quietly showing one display.
- **Primary display only**: just your main display, sized to what it shows
  full-screen, pixel for pixel. The stream does not follow the window.
- **Windowed**: your main display in a window you can resize. The image
  fits the window; on hosts that support it the remote desktop resizes with
  the window.

A host that cannot match your layout says "Match My Layout could not be
negotiated with this host". For that host, edit the connection and set its
**Displays** to **Primary display only**; other connections keep the
default.

If you reconnect to a desktop that was started without your current
layout, the Deck asks: **Reconnect to your session** keeps your open
applications on your main display only; **Start fresh with my layout**
matches your layout but closes them.

**View › Show Fullscreen** (⌃⌘F) enters and leaves full screen. On a Mac
with a notch, **Extend fullscreen under the notch** lets the remote desktop
use the strip beside the notch.

### Retina

By default the stream matches your screen's point size, so on a Retina
display each remote pixel covers four of yours. **Settings › Displays ›
HiDPI streaming (Retina)** asks for the full backing resolution instead:
sharper and 1:1, at roughly four times the bandwidth. It takes effect at the
next connection. With several displays, **Full resolution** turns it on per
display.

How big the remote desktop's own menus and text look is a separate
question:

- On a **Windows** host, **Remote UI scale** sets it. Automatic reports your
  panel's real size and lets Windows choose.
- A **Linux** host ignores Remote UI scale. Turning HiDPI streaming off
  makes its interface roughly twice as large.

## 10. How the connection is doing

<!-- source: clients/macos/src/ui/macos_menu.rs (Session menu); clients/macos/src/ui/app.rs (paint_health_overlay, host_path_row); shared/transport/src/path_signal.rs (PathGrade thresholds, path_quality_summary); docs/architecture/transport.md (path indicator); docs/operations/troubleshooting.md (Laggy or stuttering); docs/operations/error-codes.md (Connection test, diag.rtt.*) -->

**Session › Connection Health** lays a panel over the picture. The line that
matters most to you is **Path**, the network between you and the host as
the host measures it:

> Path: good · BBR · RTT 42 ms (min 38) · loss 0.1% (3 lost / 3000 sent) · 9.8 Mbit/s · cwnd 120 KB

The first word is the grade:

| Grade | Meaning |
| --- | --- |
| **good** | Round trip under 80 ms, under 30 ms of queueing, under 0.5 % loss. |
| **fair** | Usable, but distance, queueing or loss is noticeable. |
| **poor** | A round trip of 150 ms or more, 100 ms of queueing, or 2 % loss: you will feel it. |

Linux and Windows hosts report the path; other hosts show "Path: not
reported by this Pier". The other lines show frames delivered and shown,
the codec and frame size, and the Deck and host versions, which is useful
when you report a problem.

**Session › Negotiated Truth** shows the exact colour and encoding the host
is serving. The [Colour check](#4-check-your-colour) explains the same facts
in plain words.

<!-- source: shared/media/src/stats_overlay.rs (stats_overlay_lines, dynamic_range_label); clients/macos/src/ui/macos_menu.rs (Session › Stream Statistics) -->

**Session › Stream Statistics** is a smaller box for keeping an eye on a
session while you work, off until you turn it on. It has three lines: the
round trip, loss and rate of the path; the frames shown per second against
the preset's ceiling, the codec, chroma and depth, the preset the host is
serving, and **SDR** or **HDR** (read from the transfer, never from the bit
depth) with the primaries; and a **Lossless** line, which reads "Refinement
off" because this release does not turn lossless refinement on. The box
sits at the bottom left of the picture.

If things feel slow: a wired connection beats Wi-Fi, a nearer network beats
a distant VPN, and **Speed** feels faster than **Auto**. **Test connection**
also grades the round trip against the host's own limits: **good**
(`diag.rtt.good`), **slow enough to notice** (`diag.rtt.degraded`), or
**slow: drawing and typing will lag** (`diag.rtt.high`). With the default
limits that is above 60 ms and above 150 ms.

## 11. Reconnecting after a dropout

<!-- source: clients/macos/src/ui/home.rs (Auto-reconnect, default on); clients/macos/src/ui/app.rs (reconnect_screen); docs/architecture/session-auto-reconnect.md; docs/operations/error-codes.md (Session resume refusals); docs/operations/config-reference.md (auth.reconnect_window_secs); docs/operations/pier-administration.md (How long a disconnected session holds the display) -->

If the network drops, the Deck keeps the last picture, dims it, and shows
**Lost connection, attempting to reconnect** with a countdown. When the
network returns, it picks up the same session **without asking for your
password**, as long as you are back within the host's reconnect window
(three minutes unless your administrator changed it).

- **Auto-reconnect** (under the connection, on by default) controls this.
- **Disconnect** on that screen gives up and returns to your connections.
- If the host refuses to resume (the window ran out, the host restarted,
  you signed out on the host), the Deck asks for your password again.
  Nothing on the desktop is lost by that; only the shortcut is.
- It never resumes after you chose Disconnect yourself, or after the
  host's identity changed.
- On a Linux host you do not have to wait: signing in again with your
  password, from this Mac or another, takes your waiting desktop over at
  once.

## 12. Disconnecting, quitting and signing out

<!-- source: clients/macos/src/ui/macos_menu.rs (Disconnect ⌃⌥F12, Quit); clients/macos/src/ui/app.rs (request_quit, QUIT_HOLD); docs/operations/pier-administration.md (Display modes, Sessions RDP and account switching, One X11 desktop per host); docs/operations/config-reference.md (platform.session.disconnected_idle_timeout_secs, auth.reconnect_window_secs); shared/session/src/x11_seat.rs -->

- **Connection › Disconnect** (⌃⌥F12) ends the connection.
- **Quit Arcen Deck** from the menu quits. In a session, ⌘Q quits only if
  you hold it for three seconds; a tap goes to the host.

**Neither signs you out.** Your remote desktop and its applications keep
running on the host, and the next time you sign in you normally get the same
desktop back. On Linux a desktop is kept until you sign out, unless your
administrator set an idle limit.

**To end your session, sign out inside the remote desktop**, with the host's
own sign-out (the Start menu on Windows, the system menu on Linux, the Apple
menu on a Mac). Do this when you have finished on a shared workstation:
a Linux host using X11 runs one desktop at a time, and the next person is
told "Another remote desktop is active on this host (one X11 desktop per
host). Sign out of it first." Even after a plain disconnect, the host keeps
your place for the reconnect window, and someone else has to wait for it to
run out.

## 13. When something goes wrong

<!-- source: docs/operations/troubleshooting.md; docs/operations/error-codes.md; clients/macos/src/transport/websocket.rs (ConnectSmokeError texts); shared/session/src/{x11_seat,local_playback,polkit_remote}.rs; shared/input/src/text/paste.rs (NoTextRoute); clients/macos/src/ui/app.rs (NO_HDR_SCREEN) -->

Start with **Test connection** in the connection form. It needs no password
and says what to do. Then look for what you see below. Anything marked
"ask your administrator" is a host setting; send them the report from
**Copy report** or the [Colour check](#4-check-your-colour).

| You see | What it means | What to do |
| --- | --- | --- |
| The test cannot reach the host (`diag.udp.timeout`) | Nothing answers on the network port: the host is off, or a firewall or VPN blocks it. | Check you are on the right network or VPN; otherwise ask your administrator. |
| `diag.udp.local_network_unreachable` | macOS is blocking Arcen Deck from the local network. | System Settings › Privacy & Security › Local Network: turn on Arcen Deck. |
| **This host's identity changed** | The host presents a different certificate from the one you trusted. | Do not sign in. Ask your administrator; see [If the host's identity changed](#if-the-hosts-identity-changed). |
| Authentication failed | Wrong account or password, or the account is locked out. | Check the account; wait out a lockout. Arcen never checks passwords itself. |
| "The host signed you in but could not start the session" | Your password was right, but the desktop did not start. | Ask your administrator; the host's log names the cause. |
| "Another remote desktop is active on this host (one X11 desktop per host)" | Someone, possibly you elsewhere, has a desktop open on this Linux host. | They (or you) sign out there, or wait for it to time out. |
| The waiting desktop belongs to another user | Another person disconnected recently; the host keeps their place for the reconnect window. | Wait a few minutes and try again. |
| "The host could not mute its own speakers…" | The host would have played your session's sound aloud, so it refused. | Ask your administrator. |
| "The host refused this desktop: it could not keep it from local administrator rights (polkit)" | A safety check on a Linux host failed. | Ask your administrator. |
| "Match My Layout could not be negotiated with this host" | This host cannot give you one display per Mac display. | Edit the connection: **Displays** › **Primary display only**. |
| PIPELINE DEGRADED at the top of the picture | The host served a lower preset than you asked for, with its reason. | See [When the host serves less](#when-the-host-serves-less-than-you-asked-for). |
| Grading or HDR refused (`gpu_sessions_in_use`, `gpu_encoder_unavailable`, …) | The host's graphics card cannot encode that preset now, or at all. | Connect with Auto or Speed; for `gpu_sessions_in_use`, close other encoding sessions on the host. See [Grading and HDR refusals](operations/error-codes.md#grading-and-hdr-refusals). |
| HDR refused on Windows (`hdr_not_activated`, `hdr_link_exceeded`) | Windows could not switch the display to HDR, or the size is too large for HDR. | Use Grading, or turn off Retina streaming, or use one display. |
| HDR greyed out | None of your Mac's displays can show HDR. | Connect an HDR display, or use Grading. |
| The pen acts as a mouse | The tablet mode is Mouse compatibility, the host is a Linux Wayland desktop without a pen seat, or the application uses Wintab. | **Settings › Input › Tablet › Tablet support**; on Windows, set the application to Windows Ink. |
| A web page on a Mac host says *Pointer type: Mouse*, pressure 0.50 | The page is open in Safari. Safari on macOS reports every pen as a mouse, including one plugged straight into that Mac: WebKit takes no pen data from macOS. | Use Chrome on the host (it reports pen, pressure, tilt and rotation), or a native drawing application. |
| "Native tablet is not enabled on this host…" | The host's administrator has not turned Native tablet on (`usb.native_tablet`). | Choose **Tablet support**, or ask the administrator to enable it. |
| "Native tablet (USB bridged) is not offered on Windows hosts…" or "…is unavailable on this host…" | The host cannot take a bridged tablet. | Choose **Tablet support**. |
| Wrong characters, `z` and `y` swapped, `æøå` as punctuation | The host's keyboard layout differs from your Mac's. | **Connection › Keyboard: Characters** (⌃⌘U), or match the layouts. |
| "This host cannot type text sent from the Deck…" | The host cannot type characters (the Mac host, or a host older than 0.18.0). | Use physical keys; ask about updating the host. |
| Keys stuck, or every key acting like a shortcut | The host thinks a modifier is still held. | **Connection › Release Modifiers**. |
| A black screen just after a Windows sign-in | Windows waits for its desktop to settle before showing it. | Wait about 25 seconds. |
| The picture does not change | A still desktop sends no new frames; the Deck keeps the last one. | Move the pointer. If it still never updates, run Test connection. |
| Clipboard does nothing | The host's policy turns it off or one way, or the copy is too large. | Check **Settings › Clipboard › Effective host policy**. |
| No microphone on the host | Not turned on this launch, the host does not allow it, or it is a Windows host. | See [Sound and microphone](#8-sound-and-microphone). |

The full lists for administrators are [Troubleshooting](operations/troubleshooting.md)
and [Error codes](operations/error-codes.md).

## 14. Where to get help

<!-- source: SUPPORT.md; SECURITY.md; clients/macos/src/ui/app.rs (Settings › Logs, Show in folder); README.md (Test without the interface) -->

1. **Your administrator first.** They run the host, and most refusals above
   are host settings. Send them:
   - the **Copy report** text from **Test connection**;
   - for a colour question, the **Save report** file from the Colour check;
   - the Deck's log: **Settings › Logs › Show in folder** opens it. Raise
     **Log level** before you reproduce a problem; lower it again after.
2. **The project.** Arcen is free software with no company behind it and
   [no support promise](../SUPPORT.md). Bug reports with enough detail to act
   on are read, even if they wait. Never include passwords, private host
   names or customer material in a report.
3. **A security problem** goes through the private process in
   [SECURITY.md](../SECURITY.md), never a public report.
