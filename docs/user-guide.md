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
15. [Menus and keyboard shortcuts](#15-menus-and-keyboard-shortcuts)

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

<!-- source: shared/deck-core/src/screens/home.rs (the calm list, the machine's column, the two-column New and Edit connection form, Remember username, Auto-reconnect, the ＋ and ⋮ menus); shared/deck-core/src/screens/list_picker.rs (the list picker); shared/deck-core/src/presence.rs (HostPresence, NoPresence, DemoPresence); shared/deck-core/src/screens/{widgets,theme,icons,launcher}.rs (fields, buttons, the two palettes, icons, the orange bar); clients/macos/src/ui/app.rs (cert_warning_modal, certificate_changed_modal, Security modes, Review before connect, connection_serialization_never_contains_passwords); clients/macos/src/ui/connection_test.rs; shared/deck-core/src/sign_in.rs (SignInFailure messages); README.md (Connect) -->

### Home at a glance

The Deck opens on **Home**: your saved connections on the left, the
selected one on the right. It keeps out of your way on purpose:

- **The list.** Each connection shows its name and, under it, one quiet
  line. Today that line is the connection's address. The same line is
  reserved for the machine's status (**Ready**, **You're signed in ·
  resume**, **In use by someone else**, **Off**) once a host or service
  can report it; no Pier reports who is signed in today, so the
  Deck never guesses. The selected connection is tinted, with an orange bar
  at its left edge.
- **The right side.** One centred column: the machine's name (point at it
  to see its address), the **Username** and **Password** it needs, one large
  **Connect** button, **Remember username** and **Auto-reconnect** under
  it, and **Edit** as a quiet link at the bottom. A connection that keeps
  its own value for something in **On this machine** says so under
  **Connect** in one quiet line, such as **This connection: Left Option is
  Alt, keys only typing.**, with a **Use app defaults** link under it that
  hands every one of them back to Settings. Only values that differ from
  Settings are listed; with none, there is no line. When a status source
  says your own session is still signed in there, the button reads
  **Resume** and asks for no password.
- **The orange bar** at the top holds two buttons on its right, outside a
  session: **?** opens this guide, the gear opens **Settings**.
- **Light or dark.** The Deck follows your Mac's (or PC's) appearance:
  choose Light or Dark in the system settings, or Auto, and the Deck
  changes with it straight away, menus, lists and tooltips included. There
  is no separate setting in the Deck.

The **＋** beside **Connections** opens **New connection…** and **Import
from file…**. A connection's **⋮** menu, on the selected row and the row
under the pointer, has **Edit**, **Test connection**, **Export…** and
**Delete**.

Keys on Home: ⌘N on a Mac, Ctrl+N on a PC, opens **New connection**.
Return in the username or password field connects. In the connection form,
Return saves and Esc cancels, unless a list is open: then the key belongs
to the list.

### Save a connection

Your administrator gives you an address (a name such as
`workstation.example.internal`, or an address such as `203.0.113.10`) and
tells you which account to use. Arcen has no accounts of its own: you sign
in with your ordinary account on that workstation.

1. Open Arcen Deck, choose **＋** beside **Connections** at the top of the
   list, then **New connection…** (or press ⌘N on a Mac, Ctrl+N on a PC).
   With nothing saved yet, Home also has **Add connection** and **Import…**
   buttons.
2. The form has two columns, side by side when the window is wide enough
   and one under the other when it is narrow:
   - **Connection**: the **Address** (add `:port` only if you were given a
     port other than 18444, as in `workstation.example.internal:19444`; an
     IPv6 address with a port is written `[2001:db8::7]:19444`), **Name
     (optional)**, named after the address when left empty, and **Username
     (optional)**.
   - **On this machine**: what this Deck does for that connection, the **⌘
     key**, the **Option key** (on a Mac), **Typing**, **Displays** and
     **System shortcuts go to the remote computer** (**On** or **Off**).
     Each shows the value in force and one line on what it does. **App
     default (…)** means the connection follows Settings. Leave them on their
     defaults to start with; sections 6 and 9 explain them.
3. Choose **Add connection**, or press Return. If something is missing or
   wrong, the form says so under the field and puts the cursor there. Esc
   or **Cancel** leaves it. A Mac puts **Cancel** before **Add
   connection**, Windows after it, as each system's own dialogs do.
   **Test connection**, at the bottom left, checks the address you typed
   before you save it. These three stay at the bottom of the window; in a
   short window the form scrolls above them.

To change a saved connection later, choose **Edit** under its **Connect**
button, or **Edit** in its **⋮** menu.

**Choosing from a list.** Every choice from a short list, in this form and
in Settings, works the same way: click the value and the choices open in
place, pushing what follows down; the one in force is in orange with a bar
at its left. Click a choice to take it. Esc, a click elsewhere or Tab
closes the list without changing anything. From the keyboard: Tab to the
value, Down or Return opens it, Up and Down move, Return chooses. With
**Reduce motion** on (System Settings › Accessibility › Display on a Mac;
**Animation effects** off on Windows), lists and menus open at once
without sliding or fading.

### Settings

Open **Settings** with the gear at the right of the orange bar. Its
sections are on the left: **General** (Language, Streaming, Displays),
**Input and devices** (Microphone, Input, Tablet) and **System** (Logs,
Advanced). Each page is one centred column: a title, short groups with a
plain heading, a list for each choice from a short list (see **Choosing
from a list** above), a tick box for each switch with its explanation in
small grey text under it, and buttons the same size and shape as on Home.
The orange bar stays at the top, with **?** and, where the gear was,
**Done**, which goes back to Home. Changes save as you make them.

### Test before you sign in

Open a connection's **⋮** menu and choose **Test connection**; the report
shows on that connection. The New and Edit connection forms have it too,
at the bottom left. It checks the whole way to the workstation: the name,
the network port, the host's certificate, the host's greeting, the packet
size and the round trip. It sends no user name or password. Each failing
step says **What to do**. **Copy report** puts the result on the
clipboard, ready to send to your administrator.

### Sign in

Select the connection. Its name is at the top of the right side (a
warning, **Not encrypted (insecure mode)**, shows under it only when a
connection is not encrypted, which only insecure mode allows). Type your
**Username** and **Password**, and choose **Connect** or press Return. The
Deck never saves your password, and the password field is empty again once
you connect, so it is gone after you disconnect.

Signing in happens on the connection itself; there is no separate sign-in
page. If something stops it, the Deck stays on your connections with the
same connection selected, clears the password, puts the cursor in the field
to fix and says what happened under it. The message goes as soon as you
start typing, when you choose another connection, or when you open a
connection form:

- **Enter your password to connect.** Connect was chosen with no password,
  or the host asked for one.
- **Wrong username or password.** The host refused them.
- **Someone else is signed in at ‹host› (‹account›). Sign in as that user,
  or switch to your account there first.** Your password was right, but
  another account holds that computer's screen. The message sits under
  **Username**.
- **Too many failed sign-ins. Wait a few minutes, then try again.** The
  host stopped checking passwords for a while.
- **‹host› refused the sign-in. Its reason is under Details.** The host
  gave a reason the Deck does not know; **Details** has its words.
- **Couldn't reach ‹host›. Check that it's on and that Arcen is running
  there.** Nothing answered.
- **Your session ended. Sign in again to continue.** A session could not be
  resumed (see [Reconnecting after a dropout](#11-reconnecting-after-a-dropout)).

Where it helps, **Details** under the message shows the technical reason,
which is also in the Deck's log. Only something that is not about a field,
such as the host ending your session, the host restarting or the host's
identity, opens a box for you to read and close with **OK**. When the host
ends a session for its own reason, the box says that reason in plain words,
for example **‹host› couldn't keep its screen private, so it ended the
session.**; the host's code (here `console_privacy_unavailable`) is under
**Details**.

If the host shows a notice before sign-in, the Deck shows it under **Review
before connect**. Choose **Accept** to continue or **Decline** to stop.

### The first time: verify this host

The first time you connect, the Deck shows **Verify this host** with the
host's fingerprint (**SPKI SHA-256**). Compare it with the one your
administrator gives you, then choose:

- **Trust and Remember**: the Deck remembers this host and warns you if its
  identity ever changes. This is the usual choice.
- **Trust Once**: trust it only until you quit the Deck.
- **Cancel**: do not connect.

The certificate's validity is shown as dates (UTC).

### If the host's identity changed

If a remembered host presents a different certificate, the Deck stops and
shows **This computer's identity has changed**, with the fingerprint it
remembered next to the one the host presents now. It has not sent your
password. This can happen after a reinstall, or it can mean someone is
intercepting the connection: ask your administrator if you're not sure.

- **Cancel** (the default, also Return or Escape) leaves everything as it
  was.
- **Trust the new identity…** asks once more. Only when your administrator
  confirmed the change, choose **Trust and connect**: the Deck remembers the
  new identity instead of the old one and connects.

### Certificate checking

**Settings › Advanced › Certificate checking** decides how strict the Deck is.
**Medium** is the default: a certificate that no trusted authority issued
gets the **Verify this host** prompt above. **High** refuses to connect
unless a certificate authority this computer trusts issued the host's
certificate. Every Arcen host makes its own self-signed certificate, so High
refuses a host, and says why, until its administrator installs a
certificate from a trusted authority. Medium checks such a host by its
fingerprint instead. **Low** skips the check; avoid it. Settings offers it
only when Arcen Deck was started with `ARCEN_ACCEPT_INSECURE=1`; a saved
Low without it checks as Medium, and Settings says so.
Open Settings with the gear button (⚙) at the right of the orange bar at
the top of the Deck window when you are not in a session; the **?** beside
it opens this guide.

### Share a connection

<!-- source: shared/deck-core/src/profile.rs (ConnectionProfile, review_import, commit_import, MAX_PROFILE_BYTES); shared/deck-core/src/screens/profile.rs (import summary); shared/deck-core/src/screens/home.rs (Export…, Import…); clients/macos/src/support.rs, clients/windows/src/platform/support.rs (file dialogs) -->

A saved connection can travel as a small file ending in `.arcen`, for
example from an administrator to the people who use a workstation.

- **Export:** open the **⋮** menu beside a connection and choose
  **Export…**, then choose where to save the file.
- **Import:** choose **＋** beside **Connections**, then **Import from
  file…**, and pick the file.

The file holds the connection's name, address, port and username, and the
choices it keeps for itself (Typing, Option key, ⌘ key, Displays, system
shortcuts and clipboard), under the same names as in Settings. Everything it does not
set follows your own Settings, and the host's own policy still decides what
a session may do. **It never holds a password**, a token or anything that
would let someone sign in as you.

Import shows a summary before anything is saved: what will be added, which
choices the connection keeps for itself, and whether the file includes the
host's identity. Choose **Import** to save it or **Cancel** to leave
everything as it was.

- **If you already have a connection to the same address and port,** the
  Deck never overwrites it silently: choose **Replace it with this file's
  connection** or **Keep both** (the new one gets a name such as
  *Studio (2)*).
- **The host's identity is not trusted just because a file says so.** If
  the file includes one, it is shown with its fingerprint and **Trust this
  host identity** is off. Tick it only if you trust whoever sent the file
  and can compare the fingerprint with your administrator's. Left off, the
  Deck asks you to verify the host the first time you connect, as usual.
- **If the file's identity differs from the one you already trust** for
  that host, the summary shows both fingerprints and warns you. Your
  current identity stays unless you tick **Replace the identity I trust
  with the one in this file**; do that only when your administrator told
  you the host's identity changed.
- Fields a newer Arcen added are kept and written back when you export the
  connection again. Fields that look like a password, token or file path
  are ignored, and the summary says so; so are fields whose names hold
  hidden or invisible characters or run past 64 characters, which the
  summary only counts.
- **Replace keeps the address exactly as you saved it**, so the host
  identity you already trust for that connection keeps applying.

- **If another Deck window changed the host's trusted identity** while the
  summary was open, nothing is saved and the summary shows the identity
  trusted now, to decide again.

A file larger than 64 KB, nested more than 16 levels deep, written by a
newer Arcen in a format this Deck cannot read, or with a field of the wrong
kind is refused with a message saying why. Export never writes a file
another Deck would refuse: a connection carrying more than 64 KB of extra
fields is not exported, and the file already there is left as it was. What
Export and Import did shows in a short note under the connection list until
you choose **Dismiss**. The file looks like this:

```json
{
  "format": "arcen-connection",
  "version": 1,
  "name": "Studio workstation",
  "address": "workstation.example.internal",
  "port": 18444,
  "username": "alice",
  "overrides": { "typing_mode": "keys", "displays_mode": "windowed" },
  "host_identity": { "kind": "spki_sha256", "digest": "<64 hexadecimal digits>" }
}
```

Only `format`, `version`, `name` and `address` are required.

## 3. Choose a preset

<!-- source: clients/macos/src/ui/app.rs (StreamingPreset, Settings › Streaming, NO_HDR_SCREEN, without_unavailable_hdr, paint_negotiated_truth_badge); shared/media/src/video/pipeline/{auto,speed,grading,hdr}.rs (summary strings); clients/macos/src/ui/session_truth.rs (PIPELINE DEGRADED); README.md (Streaming presets and independent pipelines); docs/operations/troubleshooting.md (Wrong or washed-out colours) -->

**Settings › Streaming** offers four presets. Each one is a complete
promise about frame rate and colour, served by its own path through the
host, not a label on the same picture.

| Preset | What it promises | Use it for |
| --- | --- | --- |
| **Auto** (default) | A sharp picture at your display's refresh (60 frames a second on a 60 Hz display, 120 on a 120 Hz one where the host can show it; with Match my layout each monitor at its own), 8-bit 4:2:0, shown without tearing. When the network or the host cannot keep up it gives up frame rate, not sharpness, and climbs back when it can. | Everyday work: email, browsing, most editing. |
| **Speed** | The lowest latency: at least 60 frames a second (your display's refresh when faster), 8-bit 4:2:0, the lowest-latency encoder settings, each frame shown as soon as it is decoded, without tearing. When the network is tight it gives up sharpness. | Playback, scrubbing, animation, anything where motion matters more than fine detail. |
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
- Auto already runs at your display's refresh; Stream Statistics shows the
  rate and, when it is lower, why. If it still feels slow, try **Speed**: it
  never drops below 60 frames a second and trades sharpness, not frames,
  when the network is tight.

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
- **A Windows host with an Intel or AMD graphics card** may encode Auto and
  Speed on that card through Media Foundation; this path is **Alpha**, and
  Stream Statistics names it "Media Foundation hardware (Alpha)".

If the warning says the Deck fell back to eight-bit presentation, reconnect.
If it stays, choose Auto and send the Deck log to your administrator.

## 4. Check your colour

<!-- source: docs/operations/colour-check.md; clients/macos/src/ui/colour_check.rs; clients/macos/src/ui/macos_menu.rs (Session › Colour Check…); clients/macos/src/ui/app.rs (Settings › Streaming › Colour check…) -->

Before you judge a picture, open **Session › Colour Check…** (or
**Settings › Streaming › Colour check…** at any time). The Windows Deck
does not have it yet: its menu item is greyed and says *(not on Windows
yet)*. It is a checklist
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

### Exact still image

<!-- source: docs/adr/0016-lossless-refinement.md; shared/media/src/refine/mod.rs; shared/media/src/refine/scrgb.rs; shared/media/src/refine/ycbcr.rs; hosts/capenc/src/win_refine.rs; hosts/macos/src/refine.rs; clients/macos/src/ui/app.rs (Settings › Streaming) -->

The video stream is compressed: very good, but not the host's exact pixels.
**Exact still image** closes that gap for a picture that stops moving. When
part of the screen has been still for a moment, the host sends those exact
pixels again, losslessly, and the Deck paints them over the video and
checks them against the host's own checksum. Think of it as the host
sending a lossless screenshot of what stopped moving, piece by piece.

- **On by default.** Turn it off or on in **Settings › Streaming › Exact
  still image**, next to the colour settings. It applies from the next
  connection. When it is off, the Deck does not ask for it and nothing
  extra is sent.
- **The host allows it unless turned off.** An administrator can switch it
  off with `video.lossless_refinement: "disabled"` (see the
  [configuration reference](operations/config-reference.md)).
- **Grading and HDR only.** Auto and Speed never use it. In HDR the exact
  picture is the host's HDR (PQ) code values, shown with HDR on as usual.
- **Which hosts.** On this release, one display:

  | Host | Exact still image |
  | --- | --- |
  | Linux, X11 desktop at ten-bit (depth 30) | Grading and HDR |
  | Linux, Wayland desktop on a GNOME that serves Grading and HDR (GNOME 50 or later; see the [administration guide](operations/pier-administration.md)) | Grading and HDR |
  | Linux, Wayland on GNOME 49 or earlier | not available (no Grading or HDR there) |
  | Windows | Grading and HDR |
  | macOS | Grading and HDR (verified to level 2; needs a Deck of this release) |

  Other hosts and other sessions stream exactly as before, and Stream
  Statistics says why it is off.
- **See what you got** in **Stream Statistics**: *Exact: 100% of picture
  verified* means every pixel you see was checked to be the host's own. A
  lower number means part of the screen is still changing (a clock, a
  blinking caret) and stays video there. *Level 3* means the check was made
  on the picture actually drawn, not only on what arrived. A macOS host
  sends its picture in the video's own colour codes, which the Deck turns
  into colour exactly as it does the video, so its check ends at *level 2*
  (the codes held on the graphics card). When it is off, the line says
  why.
- **Bandwidth and time.** It sends only the parts of the screen that are
  still, at up to half of what the connection has to spare (measured once
  a second, and paused whenever the video needs the link, the connection
  loses packets or its queue grows), and a part that moves again simply
  stays video; motion is not slowed by it, and the video's own bitrate is
  not lowered for it. Once the
  picture is still it costs nothing more. How long the exact picture takes
  depends on how much detail it has. Measured over a fast local network:

  | Still picture | Sent once | Exact and verified after |
  | --- | --- | --- |
  | 1080p, windows, text and colour ramps | 0.3 MB | under 1 second |
  | 1080p, full-screen photograph | 2.5 MB | about 2.5 seconds |
  | 4K, windows, text and colour ramps | 0.9 MB | under 2 seconds |
  | 4K, full-screen photograph | 10 MB | about 6.5 seconds |

  The picture fills in progressively; *Exact: arriving, N% of picture*
  shows how far it has got.

## 5. Pen and tablet

<!-- source: shared/deck-core/src/screens/input_choices.rs (tablet_choices, TABLET_FOOTER); shared/deck-core/src/screens/settings.rs (Settings › Tablet); clients/macos/src/ui/app.rs (set_tablet_mode_requested); clients/macos/src/ui/macos_menu.rs (View › Tablet Monitor); shared/input/src/negotiation.rs and hosts/windows/src/session.rs (refusal texts); docs/adr/0017-forward-only-pen-input.md; docs/adr/0018-unified-typed-devices.md; docs/operations/pen-tablet-input.md; README.md (Pen and tablet) -->

Any pen tablet works, whoever made it. Choose the mode in **Settings ›
Tablet**, directly under Input:

| Choice | In one line |
| --- | --- |
| **Tablet support** (Recommended) | Your pen works as a pen on the remote computer. |
| **Native tablet** | The remote computer's tablet driver talks to your tablet. |
| **Mouse only** | Your pen moves the pointer like a mouse. |

The choice applies to every connection; changes apply from the next
connection. (Before 0.20.7 each saved connection kept its own mode; 0.20.7
starts everyone on Tablet support.) Which one to choose comes down to the
network between you and the host: Tablet support for Wi-Fi, 5G and working
away from the office, Native tablet only on an office network.

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
- On the Windows Deck, the tablet's driver must hand the pen to Windows as a
  pen. When it does not, the Deck says: "Your tablet is sending mouse
  input, so pressure can't reach the host. In Wacom Tablet Properties ›
  Mapping, turn on Use Windows Ink for this app (or Other Programs)."

What the host makes of the pen:

| Host | What applications see |
| --- | --- |
| Windows | A **Windows Ink** pen, with 1,025 pressure steps. Set each drawing application to Windows Ink (or "pointer input", "Real-Time Stylus"). **Wintab is not supported**: an application set to Wintab gets no pressure. The [per-application settings](operations/pen-tablet-input.md#4a-windows-applications-choose-windows-ink) list where to switch. |
| Linux using X11 | A virtual tablet with pressure, tilt, eraser and barrel buttons. |
| Linux using Wayland | A real pen with pressure and tilt when the desktop runs on its own seat; on other Wayland desktops the pen acts as a mouse for now. |
| Mac (preview) | The tablet events a Mac tablet driver posts: proximity, pressure, tilt, rotation, eraser, barrel buttons. At the Mac sign-in window the pen is a plain pointer. |

**View › Tablet Monitor** shows what the Deck sees from your pen and whether
the host accepted it as a pen. Left-handed use, the tablet's area and its
pressure curve are set in the tablet maker's settings on your Mac, as for
every other application; Arcen follows them.

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
- **Mac hosts**: any tablet that reports a standard pen, with pressure,
  tilt, buttons and eraser as far as the tablet reports them; models with a
  dedicated reader (the Wacom Intuos5 touch L today) get more. A tablet
  whose pen speaks only its maker's own format is refused with a reason.
- **Windows hosts**: the same as Mac hosts, with nothing installed on the
  host: the Pier reads the pen itself and gives it to Windows as a Windows
  Ink pen, so set drawing applications to Windows Ink (pressure in Windows
  Ink's 1/1024 steps).
- A host that cannot take it says why ("Native tablet is not enabled on this
  host…", "…is unavailable on this host…"), and you keep ordinary mouse control. Choose **Tablet support**
  instead, or ask the host's administrator.

When you disconnect, the helper hands the tablet back to the Mac as if you
had unplugged it and plugged it in again, so Mac applications and the
tablet maker's driver get it back.

If you do use it, it needs macOS 13 or later and the Arcen USB component, a
small helper (on Intel Macs it has not been tried with a real tablet yet).
**Settings › Tablet** installs it (**Install component…** beside
Native tablet), and you approve it once in System Settings › General › Login
Items. The button beside Native tablet always says where the component
stands. The Deck itself never runs with
extra privileges. The host's own driver moves the host's pointer, and the
Deck draws the pen's pointer where the host reports it, without waiting for
the video. Moving the Mac's mouse brings its own pointer back at once. A
host that cannot report its pointer (older versions) leaves the bridged pen
without a visible pointer.

The project's direction is to describe each device by what it can do, so
the host builds a matching virtual pen (or other device) without a driver.
That design is [ADR 0018](adr/0018-unified-typed-devices.md); it is not
built yet, and nothing about Tablet support changes because of it.

### Mouse only

No pen redirection: the pen moves the pointer like a mouse, with no
pressure, tilt, eraser, proximity or tablet buttons, and nothing
tablet-specific is needed on either side.

## 6. Keyboard

<!-- source: shared/input/src/keyboard/{capture.rs (KeyboardCapture), switch_chord.rs (KeyboardSwitchChord, check), deck_shortcuts.rs (DeckShortcut), mac_capture.rs (MacTapRouter), send_keys.rs (SendKey), host_keys.rs}; shared/deck-core/src/screens/keyboard_help.rs (status line, notice, switch help, Help sheet, ⌘Tab question); shared/deck-core/src/screens/input_choices.rs (Settings › Input, Advanced › Input, Edit Connection › On this machine); shared/deck-core/src/screens/settings.rs (Settings › Advanced › Input › Keyboard switch); clients/macos/src/ui/{app.rs, keyboard.rs, system_shortcut_tap.rs, macos_menu.rs, home.rs, text_input.rs, paste_keys.rs}; clients/windows/src/input/{mod.rs, hook.rs}; clients/windows/src/shell/{launcher.rs, menu_bar.rs}; shared/input/src/text/{paste.rs, policy.rs}; shared/input/src/pen_buttons.rs -->

Most keys simply go to the remote computer. This section explains the one
thing to learn, the **keyboard switch**, then the keys the Deck keeps, the
keys your keyboard may lack, and what differs on a Mac and per host.

### The keyboard switch: Remote or Local

The Deck has one switch for the keyboard. Press **Ctrl+Alt+Home** (on a Mac
**⌃⌥Home**; on a laptop without Home, Fn+⌃⌥←) to flip it.

| | **Remote** (the default) | **Local** |
| --- | --- | --- |
| Typing and ordinary shortcuts | go to the remote computer | stay on this computer |
| Alt+Tab, Alt+Shift+Tab | switch the remote computer's windows | switch this computer's windows |
| The Windows key, Print Screen | open the remote launcher, reach the remote application | act on this computer |
| On a Mac: ⌘Tab, ⌘Space, Mission Control (⌃↑, ⌃↓), ⌃←, ⌃→ | go to the remote computer, when [the ⌘Tab switch](#on-a-mac) is on | act on this Mac (app switcher, Spotlight, Spaces) |
| On a PC: a shortcut another app claims for itself (Gemini in Chrome's Alt+G) | goes to the remote computer when the system shortcuts do | goes to that app |

- **Clicking the picture** in Local also returns to Remote. That click is
  not sent, so it never paints a stroke or presses a button on the remote
  computer.
- Going to Local first lets go of every key held on the remote computer, so
  nothing stays stuck down there.
- Every flip shows a short notice at the top of the window: **Keyboard:
  Remote — Alt+Tab goes to Z8** (⌘Tab on a Mac), or **Keyboard: This
  computer**.
- The indicator keeps saying where the keyboard is: at the right end of the
  menu bar (**Keyboard: Z8 (Ctrl+Alt+Home to release)** or **Keyboard: this
  PC (click to return)**), and in fullscreen on a PC the connection bar,
  which stays open while the keyboard is Local.
- **Connection › Keyboard: Host or This Computer** flips it from the menu.
- Each new session starts in Remote.

**Settings › Input › Keyboard › System shortcuts go to the remote
computer** decides whether the system shortcuts follow the switch. On, in
Remote they go there, windowed or fullscreen; off, they always stay on this
computer, and other keys still go to the remote computer in Remote. On a
PC the switch also covers a shortcut another app claims for itself: an app
may claim a chord (Gemini in Chrome claims Alt+G, a chord Autodesk Flame
uses), and Windows hands it to that app before any window sees it. On, the
Deck takes such a chord, any key pressed with Ctrl, Alt or the Windows key,
before that app can; off, the app gets it. Some keys stay on the PC even
then:

- Ctrl+Alt+Del and Windows+L: Windows never hands them to an app.
- A chord typed with `AltGr` (or Ctrl+Alt, where the keyboard layout has
  `AltGr`) types its character, unless **AltGr** is set to act as Alt.
- While **Typing** sends characters, the PC's input method keeps its own
  keys: Ctrl+Space and Alt+\` switch it on or off, and while it composes
  (the underlined text before you confirm it), every key is its own.
  Microsoft Pinyin also keeps Ctrl+. and Ctrl+Shift+F.
- Another app whose own keyboard hook blocks a chord still keeps it: the
  Deck is never a way around such a hook.

On a Mac the Deck takes only the shortcuts listed under
[⌘Tab, ⌘Space and Mission Control](#tab-space-and-mission-control); a
chord another Mac app claims (Gemini in Chrome's ⌃G) goes to that app. It is
on by default on a PC (Alt+Tab, the Windows key and Print Screen) and off by
default on a Mac, where it needs the Accessibility permission
([⌘Tab, ⌘Space and Mission Control](#tab-space-and-mission-control)).
0.20.7 made this one switch: **The remote computer when fullscreen** became
on.
A connection can keep its own choice (**Edit** › **On this machine** ›
**System shortcuts go to the remote computer**: **On** or **Off**); Settings
then says how many connections use their own, and Home lists it in that
connection's summary line.

### Choosing another chord for the switch

If Ctrl+Alt+Home is awkward on your keyboard, or a remote application needs
it, choose another one in **Settings › Advanced › Input › Keyboard
switch** (Settings › Input names the chord in use):

- **Ctrl+Alt+Home** (on a Mac Control-Option-Home), the default.
- **Ctrl+Alt+Shift+Z**.
- **Alt+F1** (on a Mac Option-F1). GNOME uses Alt+F1 to open its
  Activities overview, so a GNOME desktop no longer gets it.
- **Custom chord**: type it, for example `Ctrl+Alt+K` or `Ctrl+Shift+Win+F2`
  (on a Mac, Win means ⌘ and Alt means ⌥).

The Deck checks a custom chord at once and keeps the previous one if it
cannot work. It refuses a chord the operating system keeps for itself
(Win+L, Ctrl+Alt+Del, ⌥⌘Esc, ⌃⌘Q), a plain key or Shift and a key (that is
typing), a chord the Deck already uses for something else, and a chord
macOS acts on before any app sees it (⌘Tab, ⌘Space, ⌃ and an arrow, the
screenshot keys). It warns, but allows it, when the chord is one that a
professional application such as Flame, Maya, Nuke or Resolve uses: that
application then never gets it. The choice applies to both Decks the same
way, and the menus, the indicator and Help › Keyboard Shortcuts show the
chord you chose.

### Send Key

**Connection › Send Key** sends a key your keyboard lacks or the operating
system would not pass on: Print Screen, Scroll Lock, Pause, Insert,
Ctrl+Alt+Del, Menu and F13–F24. Each is one press and release, after every
key held on the remote computer is let go. An item is greyed when the
remote computer cannot take that key.

### The keys the Deck keeps

Everything else reaches the remote computer, including F9 and the keys
professional applications rely on. The Deck keeps only:

| PC keyboard | Mac keyboard | What it does |
| --- | --- | --- |
| Ctrl+Alt+Home (or your chosen chord) | ⌃⌥Home (or your chosen chord) | The keyboard switch |
| Ctrl+Alt+F12 | ⌃⌥F12 | Disconnect |
| Ctrl+Alt+Break or Ctrl+Alt+Enter | ⌃⌘F | Fullscreen on or off |
| Ctrl+Alt+End | Connection › Send Key › Ctrl+Alt+Del | Send Ctrl+Alt+Del to the remote computer |
| Alt+F4, held | ⌘Q, held for three seconds | Quit the Deck (a tap goes to the remote computer) |

Releasing stuck keys, the keyboard modes and Paste as Keystrokes are menu
items without a shortcut.

### On a Mac

#### Your Mac keys on the host

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

Each saved connection can change this (**Edit** › **On this machine** ›
**⌘ key**; the line under it says what the choice in force does, and the
table above is the full mapping):

- **App default (Automatic)**, the default, as in the table.
- **⌘ is the Windows key, Mac edit shortcuts**: the Windows App behaviour.
- **⌘ and Control swapped**: ⌘ is Ctrl for every key, Control is the
  Windows key.
- **⌘ is Control**: ⌘ and Control are both Ctrl.

A Mac host always keeps ⌘ as ⌘. On the Windows Deck the same choice is
**Command key on a Mac host**: **Ctrl, as on Windows** (the default) or
**Windows key, keys as labelled**.

The ⌥ keys are a choice of each connection too (**Edit** › **On this
machine** › **Option key**), not of Settings:

- **App default (Automatic)**: a Mac host keeps both Option keys as Option;
  on other hosts left ⌥ is Alt and right ⌥ types this Mac's special
  characters (AltGr).
- **Left Option is Alt, right Option types characters**.
- **Both Option keys are Alt**: both send Alt shortcuts.
- **Both Option keys type characters**: both type this Mac's special
  characters.

0.20.7 removed the **Left Option is also AltGr** checkbox and moved the ⌥
choice out of Settings; everyone starts on Automatic.

#### ⌘Tab, ⌘Space and Mission Control

macOS acts on ⌘Tab, ⌘Space, Mission Control and ⌃←/⌃→ before any window
sees them. To send them to the remote computer in Remote, the Deck needs the
**Accessibility** permission:

- The first time ⌘Tab takes you out of a session in Remote, the Deck asks
  once: **Send ⌘Tab to the remote? macOS needs the Accessibility
  permission.** **Allow** turns the sending on and macOS asks you to allow
  Arcen Deck in **System Settings › Privacy & Security › Accessibility**.
  **Not now** is remembered and the Deck does not ask again.
- You can turn it on or off at any time in **Settings › Input › Keyboard ›
  System shortcuts go to the remote computer**. While it is on without the
  permission, the switch says **Needs Accessibility permission.** and its
  **Open System Settings…** button opens the request.
- Each connection can keep its own choice: **Edit** › **On this machine**
  › **System shortcuts go to the remote computer**, **App default (…)**,
  **On** or **Off**. Settings counts the connections that do ("1 connection
  uses its own"). The permission is one for the whole app: a connection set
  to **On** without it gets the same question the first time ⌘Tab leaves its
  session, and Settings shows **Needs Accessibility permission.** while any
  connection sends them. On a PC the same choice decides whether Alt+Tab,
  the Windows key and Print Screen go to that connection's computer.
- Without the permission nothing changes: these chords act on this Mac, and
  the indicator says **Keyboard: Z8, system shortcuts on this Mac**.

With it, the Deck watches the keyboard only while the switch is Remote and a
session window is in front, and takes only these chords:

| On this Mac | On the remote computer |
| --- | --- |
| ⌘Tab, ⇧⌘Tab | the app switcher (Alt+Tab on Windows and Linux, ⌘Tab on a Mac) |
| ⌘\`, ⌘Space, ⌥⌘Space, ⌥⌘D, ⇧⌘3, ⇧⌘4, ⇧⌘5 | the same keys, mapped like any other |
| ⌃↑ (Mission Control), ⌃↓ | Ctrl+↑, Ctrl+↓ |
| ⌃←, ⌃→ | Ctrl+←, Ctrl+→ (in Local they switch this Mac's Spaces) |

⌃← and ⌃→ follow the switch like every other system shortcut: to move
between this Mac's Spaces during a session, press ⌃⌥Home first. (Before
this update a separate setting decided them; it is gone.)

macOS never lets the Deck take ⌥⌘Esc (Force Quit) or ⌃⌘Q (Lock Screen),
and while a password field anywhere on this Mac asks for **Secure Input**,
macOS hands no key to the event tap at all: those keys act on this Mac
until the password field lets go.

#### Print Screen, Scroll Lock and Pause

A Mac keyboard has no Print Screen, Scroll Lock or Pause key. On a Windows
or Linux host, **F13**, **F14** and **F15** are those keys: an Apple
extended keyboard has them in those places, and macOS reports a PC
keyboard's Print Screen, Scroll Lock and Pause as F13–F15. A Mac host gets
F13–F15. Without those keys, use **Connection › Send Key**.

F1–F12 reach the remote computer as function keys when you hold Fn, or turn
on **Use F1, F2, etc. keys as standard function keys** in this Mac's
keyboard settings.

### Characters or keys

Normally the host's keyboard layout decides each character, which is right
for shortcuts and games. When the host's layout differs from your Mac's
(say, a US layout on the host under a Norwegian Mac keyboard), switch the
session to characters:

| Connection menu | What reaches the host |
| --- | --- |
| **Keyboard: Physical Keys** | Keys by position, as the Typing setting says. |
| **Keyboard: Characters** | Every character as your Mac typed it (`æøå`, accents, ⌥ characters, input methods), whatever the host's layout. |

These are menu items without a shortcut: ⌃⌘K and ⌃⌘U go to the host
(after Arcen 0.20.3).

In Characters mode the picture shows a small **Keyboard: Characters**
badge. Shortcuts, arrows, function keys, Esc, Return, Tab and Backspace stay
keys. A game or a virtual-machine console that reads key positions may
misbehave; switch back with **Connection › Keyboard: Physical Keys**. A saved connection can keep its
own Typing (**Edit** › **On this machine** › **Typing**). Characters mode needs a Windows or
Linux host from Arcen 0.18.0; a host that cannot type text says so and
keeps physical keys.

**Settings › Input › Typing** sets the everyday behaviour for every
connection:

- **Characters** (Recommended) sends the letters you type, whatever the
  remote keyboard layout: keys go as keys, except where the host's layout
  would type a different character; those, and whatever your input method
  composes (accents, dead keys, Japanese, Chinese, emoji), arrive as the
  characters you typed. It needs a host that types text.
- **Keys only** sends physical keys, for games and apps that read key
  positions; the host's layout decides each character and the input method
  does not compose.

The line beside it says how many saved connections keep their own Typing
("Default for all connections. 2 connections use their own."); a
connection's **Edit** › **On this machine** › **Typing** shows **App default
(Characters)** while it follows Settings. (0.20.7 dropped **Always
characters**: it became Characters; **Connection › Keyboard: Characters**
still types every character as text for a session.)

### Paste as Keystrokes

Clipboard sharing does not reach a sign-in screen, a lock screen, a
Windows elevation prompt or a virtual-machine console. **Connection › Paste
as Keystrokes** types your Mac clipboard's plain text there instead (a
menu item; ⌃⌥⌘V goes to the host).

- It waits until you let go of every modifier key ("Release the modifier
  keys to start typing"), so no character becomes a shortcut.
- It types 25 characters a second by default. Slow it down in **Settings ›
  Advanced › Input › Paste as Keystrokes › Speed** for virtual-machine
  consoles and older applications that drop keys.
- It asks first above 2,000 characters (**Ask before pasting more than**,
  in the same place) and never types more than 100,000.
- **Esc** or **Connection › Stop Typing** stops it. Keys you press while it
  types are not sent.
- A line break at the very end is not typed, so a pasted password never
  submits a form on its own. Press Return yourself.
- The text is never logged. A host whose clipboard policy keeps your Mac's
  clipboard out keeps it out of Paste as Keystrokes too.

It needs a host that types text (Windows or Linux from Arcen 0.18.0). The
Mac host does not type text yet and says so.

### Pen buttons

The pen's side buttons are part of the pen, not the keyboard: see
[Pen and tablet](#5-pen-and-tablet). In **Settings › Tablet**, under
**Tablet support**, choose what each button does on the remote computer (Right
click, Middle click or Off): the Windows Ink barrel button on a PC, the
lower and upper side switch of a Mac pen. When the tablet driver itself
turns a button into a click, the Deck sends that click.

### The pointer

Your mouse always controls the remote computer, and the pointer is
automatic; there is no setting for it. A host that sends its pointer's
shapes lets this Mac or PC draw the pointer, so it moves at the speed of
your hand in the host's own shapes; a Mac host sends them from the next
release. A 0.20.7 or 0.20.8 Mac host, or a newer one that cannot read its
pointer, draws the pointer into the picture, one video round trip later, and
the Deck shows none of its own. Whenever a host draws the pointer into the
picture, the Deck hides its own, so you never see two. Linux and Windows
hosts, and Mac hosts older than 0.20.7, keep the cursor choice you had
before 0.20.7 (the Deck draws the pointer, unless you had chosen the
host's).

### What never goes to the remote computer

Some keys belong to this computer's operating system, which acts on them
before any application, the Deck included, can see them:

- **Win+L** locks this PC. To lock the remote computer, use its own menu.
- **Ctrl+Alt+Del** opens this PC's security screen. Use **Connection › Send
  Key › Ctrl+Alt+Del** (or Ctrl+Alt+End on a PC) for the remote computer's.
- On a Mac, **⌥⌘Esc** (Force Quit) and **⌃⌘Q** (Lock Screen) always stay on
  this Mac, and so does every key while another application asks for secure
  input (a password field elsewhere turns on macOS's Secure Input).

### Notes per remote computer

- **Linux with GNOME**: Print Screen opens GNOME's screenshot tool, as it
  would at the desk, and Alt+F1 opens the Activities overview. An
  application that wants Print Screen itself needs it unbound in GNOME's
  keyboard settings.
- **Windows**: Ctrl+Alt+Del arrives through Send Key or Ctrl+Alt+End.
  F13–F24 and the Menu key arrive as Windows' own keys.
- **Mac (preview)**: macOS has no F21–F24, Insert, Print Screen, Scroll Lock
  or Pause, so the Mac host takes F13–F20 and the Menu key only, and Send
  Key greys out the rest. Your ⌘ stays ⌘.

### Help › Keyboard Shortcuts

**Help › Keyboard Shortcuts** lists every key the Deck keeps, the keyboard
switch with the chord you chose, the menu items and the keys Send Key
sends, so you can check it during a session.

### Two keys to remember

- **⌘Q in a session goes to the host.** Tap it and the host application
  gets it. Hold it for three seconds to quit the Deck. Choosing **Quit Arcen
  Deck** from the menu quits at once.
- **Connection › Release Stuck Keys** lets go of every key and button the
  host still thinks is held, and of the pointer lock, if keys start acting
  strangely after switching apps. (F9 did this up to Arcen 0.20.3; F9 now
  reaches the host.)

## 7. Clipboard

<!-- source: shared/deck-core/src/screens/input_choices.rs (Settings › Input › Clipboard); shared/deck-core/src/clipboard.rs (HostClipboard, "Clipboard is turned off by this host."); docs/security/clipboard-redirection.md; docs/operations/troubleshooting.md (Clipboard does not work); README.md (What a session carries) -->

Copy on one side, paste on the other. Text and images work in both
directions, as far as the host allows: the host decides the direction, the
content and the size limit, and the Deck cannot override it.

- **Settings › Input › Clipboard › Share the clipboard with remote
  computers** is your consent, on by default. Off, the Deck offers the host
  no clipboard at all and never reads yours.
- A connection profile can keep the clipboard off for one machine; the line
  under the switch says how many connections do ("1 connection uses its
  own"). No connection can turn it on when you turned it off.
- The host has the last word. When it turns the clipboard off, the session
  says **Clipboard is turned off by this host.**: on a Mac in the menu of
  the keyboard indicator at the right of the menu bar, on a PC beside the
  keyboard indicator in the menu bar and the fullscreen connection bar. It
  is not a setting you can change on the Deck.
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

On every host **the host's own speakers are muted by default** for your
whole session, so nobody next to the machine hears your work. If the host
cannot prove its speakers are silent, it refuses the session ("The host
could not mute its own speakers…") rather than play aloud. Your
administrator can choose to let them play. A Mac host needs the System Audio
Recording permission for that, and refuses the session until it has it.

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
  needs a host that supports it, and on a Mac, macOS's "Displays have
  separate Spaces" turned on and **Extend fullscreen under the notch**
  turned off. If
  it cannot be done, the connection fails with a message that says why
  rather than quietly showing one display. A display you turned on its side
  (portrait) gets a portrait display on the host; a host that cannot rotate
  a display gives it the same portrait size, upright.
- **Primary display only**: just your main display, sized to what it shows
  full-screen, pixel for pixel. The stream does not follow the window.
- **Windowed**: your main display in a window you can resize. The image
  fits the window; on hosts that support it the remote desktop resizes with
  the window.

A host that cannot match your layout says "Match My Layout could not be
negotiated with this host" and offers **Use Primary Display Only**: one
display for that session only, and the next connection asks for your
whole layout again. To keep that host on one display, edit the connection
and set its **Displays** to **Primary display only**; other connections
keep the default.

A connection's own **Displays** choice wins over Settings › Displays. Its
**Edit** › **On this machine** › **Displays** shows **App default (Match
my layout)** while it follows Settings, or the choice it keeps. A
connection that keeps its own says so on Home, under **Connect**, in its
summary line (**This connection: Opens in a window.**), with a **Use app
defaults** link that hands it, and the connection's other own settings,
back to Settings (a connection that follows Settings shows no such line). Settings › Displays says it too ("‹name› has its own Displays
choice, …"), with a **Use this setting for ‹name›** button.

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

- On a **Windows** host, **Remote UI scale** sets it, for each display.
  Automatic reports your panel's real size and lets Windows choose. Windows
  has only fixed steps, so a display may show the nearest one (125% on a
  1.5× Retina display shows at 175%); the Deck says so.
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
depth) with the primaries, followed by the host's encoder (for example
"NVIDIA NVENC", or "Media Foundation hardware (Alpha)"); and a **Lossless** line, which reads "Refinement
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
  you signed out on the host), the Deck returns to your connections with
  the same connection selected and asks for your password there: **Your
  session ended. Sign in again to continue.** A host that restarted says
  so in a box first. Nothing on the desktop is lost by that; only the
  shortcut is.
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
| **This computer's identity has changed** | The host presents a different certificate from the one you trusted. | Do not sign in. Ask your administrator; see [If the host's identity changed](#if-the-hosts-identity-changed). |
| **Couldn't reach ‹host›. Check that it's on and that Arcen is running there.** | Nothing answered the connection: the host is off, Arcen is not running there, or the network is in the way. | Run **Test connection**; check your network or VPN; otherwise ask your administrator. |
| **Wrong username or password.** | Wrong account or password, or the account is locked out. | Check the account; wait out a lockout. Arcen never checks passwords itself. |
| **Someone else is signed in at ‹host› (‹account›).** | Your password was right, but another account holds that computer's screen (`console_other_user`). | Sign in as that account, or switch to yours at the computer first. |
| **‹host› couldn't keep its screen private, so it ended the session.** | The host could not blank its own screens, speakers and input for you (`console_privacy_unavailable`), so it would not stream. | Ask your administrator; **Details** has the host's reason. |
| **Too many failed sign-ins. Wait a few minutes, then try again.** | The host refuses sign-ins from you for a while after several wrong passwords. | Wait a few minutes; check the password. |
| **Couldn't start a session on ‹host›.** | Your password was right, but the desktop did not start. **Details** in the box gives the host's reason. | Ask your administrator; the host's log names the cause. |
| "Another remote desktop is active on this host (one X11 desktop per host)" | Someone, possibly you elsewhere, has a desktop open on this Linux host. | They (or you) sign out there, or wait for it to time out. |
| The waiting desktop belongs to another user | Another person disconnected recently; the host keeps their place for the reconnect window. | Wait a few minutes and try again. |
| "The host could not mute its own speakers…" | The host would have played your session's sound aloud, so it refused. | Ask your administrator. |
| "The host refused this desktop: it could not keep it from local administrator rights (polkit)" | A safety check on a Linux host failed. | Ask your administrator. |
| "Match My Layout could not be negotiated with this host" | This host cannot give you one display per Mac display. | Edit the connection: **Displays** › **Primary display only**. |
| PIPELINE DEGRADED at the top of the picture | The host served a lower preset than you asked for, with its reason. | See [When the host serves less](#when-the-host-serves-less-than-you-asked-for). |
| Grading or HDR refused (`gpu_sessions_in_use`, `gpu_encoder_unavailable`, …) | The host's graphics card cannot encode that preset now, or at all. | Connect with Auto or Speed; for `gpu_sessions_in_use`, close other encoding sessions on the host. See [Grading and HDR refusals](operations/error-codes.md#grading-and-hdr-refusals). |
| HDR refused on Windows (`hdr_not_activated`, `hdr_link_exceeded`) | Windows could not switch the display to HDR, or the size is too large for HDR. | Use Grading, or turn off Retina streaming, or use one display. |
| HDR greyed out | None of your Mac's displays can show HDR. | Connect an HDR display, or use Grading. |
| The pen acts as a mouse | The tablet mode is Mouse only, the host is a Linux Wayland desktop without a pen seat, or the application uses Wintab. | **Settings › Tablet › Tablet support**; on Windows, set the application to Windows Ink. |
| No pressure from a Windows Deck; the Deck said "Your tablet is sending mouse input…" | The tablet's driver gives the Deck mouse input, not a pen (Wacom: Use Windows Ink is off for the Deck). | In Wacom Tablet Properties › Mapping, turn on **Use Windows Ink** for Arcen Deck (or Other Programs). |
| A web page on a Mac host says *Pointer type: Mouse*, pressure 0.50 | The page is open in Safari. Safari on macOS reports every pen as a mouse, including one plugged straight into that Mac: WebKit takes no pen data from macOS. | Use Chrome on the host (it reports pen, pressure, tilt and rotation), or a native drawing application. |
| "Native tablet is not enabled on this host…" | The host's administrator has not turned Native tablet on (`usb.native_tablet`). | Choose **Tablet support**, or ask the administrator to enable it. |
| "Native tablet (USB bridged) unavailable…" or "…refused…" | The host cannot take a bridged tablet, or reads no pen from this one; the reason says which. | Choose **Tablet support**. |
| Wrong characters, `z` and `y` swapped, `æøå` as punctuation | The host's keyboard layout differs from your Mac's. | **Connection › Keyboard: Characters**, or match the layouts. |
| "This host cannot type text sent from the Deck…" | The host cannot type characters (the Mac host, or a host older than 0.18.0). | Use physical keys; ask about updating the host. |
| Keys stuck, or every key acting like a shortcut | The host thinks a modifier is still held. | **Connection › Release Stuck Keys**. |
| Alt+Tab or Print Screen act on the Windows Deck's own PC | This PC has the keyboard (the menu bar says **Keyboard: this PC**), or Settings › Input keeps the system shortcuts here. | Click the picture or press Ctrl+Alt+Home; turn on **Settings › Input › System shortcuts go to the remote computer**. |
| A shortcut such as Alt+G opens another app (an assistant such as Gemini) instead of reaching the host | That app claimed the chord. On a PC the system shortcuts stay on this PC (the switch is off, or this PC has the keyboard); a Mac Deck never takes such a chord. | On a PC, turn on **Settings › Input › System shortcuts go to the remote computer** and click the picture or press Ctrl+Alt+Home. Anywhere, change or turn off that app's shortcut (Gemini in Chrome: Chrome › Settings › AI innovations › Gemini in Chrome). |
| ⌘Tab or ⌘Space act on the Mac, not the host | The switch is off (the default), Arcen Deck is not allowed in Accessibility, this Mac has the keyboard, or a password field elsewhere holds Secure Input. | Turn on **Settings › Input › System shortcuts go to the remote computer** and allow Accessibility; click the picture or press ⌃⌥Home. |
| Nothing I type reaches the host (Mac Deck) | This Mac has the keyboard (the menu bar says **Keyboard: this Mac**). | Click the picture or press ⌃⌥Home. |
| A black screen just after a Windows sign-in | Windows waits for its desktop to settle before showing it. | Wait about 25 seconds. |
| The picture does not change | A still desktop sends no new frames; the Deck keeps the last one. | Move the pointer. If it still never updates, run Test connection. |
| Clipboard does nothing | **Share the clipboard** is off, the host's policy turns it off or one way, or the copy is too large. | Check **Settings › Input › Clipboard**; the session says **Clipboard is turned off by this host.** when the host blocks it. |
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

## 15. Menus and keyboard shortcuts

<!-- source: shared/deck-core/src/screens/menu.rs (deck_menus, MenuCommand titles, render_connection_bar, QUIT_HOLD); shared/input/src/keyboard/deck_shortcuts.rs (DeckShortcut chords); clients/macos/src/ui/macos_menu.rs (the application menu, key equivalents); clients/windows/src/shell/menu_bar.rs; shared/deck-core/src/screens/launcher.rs (the orange bar) -->

Both Decks list the same commands. On a Mac they are in the menu bar at the
top of the screen; on Windows in the window's menu bar and, in fullscreen,
in the **connection bar**, which appears when the pointer reaches the top
edge of the screen (or stays while Ctrl+Alt+Home has given this PC the
keyboard) and draws nothing over the picture otherwise. Commands that need
a session are greyed outside one. Outside a session the orange bar at the
top of the window has **?** (this guide) and the gear (**Settings**); in
Settings, **?** and **Done**.

**Arcen Deck** (the Mac's application menu)

| Item | Shortcut |
| --- | --- |
| About Arcen Deck | |
| Services | |
| Hide Arcen Deck | ⌘H |
| Hide Others | ⌥⌘H |
| Show All | |
| Quit Arcen Deck | ⌘Q (in a session, hold it for 3 seconds; a tap goes to the host) |

**Connection**

| Item | Mac | Windows |
| --- | --- | --- |
| Disconnect | Control-Option-F12 | Ctrl+Alt+F12 |
| Release Stuck Keys | | |
| Send Ctrl+Alt+Del | (in Send Key) | Ctrl+Alt+End |
| Keyboard: Host or This Computer | Control-Option-Home, or the chord chosen in Settings › Advanced | Ctrl+Alt+Home, or the chord chosen |
| Send Key › Print Screen, Scroll Lock, Pause, Insert, Ctrl+Alt+Del, Menu, F13 to F24 | | |
| Paste as Keystrokes | | |
| Stop Typing | Esc while typing | Esc while typing |
| Keyboard: Physical Keys | | |
| Keyboard: Characters | | |
| Exit (Windows) | | Alt+F4 (in a session, hold it for 3 seconds) |

**View**

| Item | Mac | Windows |
| --- | --- | --- |
| Show Fullscreen | Control-Command-F | Ctrl+Alt+Break |
| Pointer Lock | | |
| Tablet Monitor | | |

**Session**

| Item | Note |
| --- | --- |
| Connection Health | |
| Stream Statistics | |
| Lossless Tile View | |
| Negotiated Truth | |
| Colour Check… | Mac only for now; Windows shows it as "(not on Windows yet)" |

**Help**

| Item | Note |
| --- | --- |
| About Arcen Deck | Windows; on a Mac it is in the application menu |
| Keyboard Shortcuts | the list of every key the Deck keeps |

On Home: ⌘N (Ctrl+N on Windows) opens **New connection**; Return connects
from the username or password field and saves the connection form; Esc
cancels the form or closes an open list first.
