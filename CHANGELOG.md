# Changelog

All notable changes to Arcen are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Arcen uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.21.4] — 2026-11-25

Your desktop keeps running while it is locked and locks when your Deck
leaves, and Match My Layout fits every connection to your Deck's monitors,
on a desktop another product owns too. On Windows, HDR on a PC without HDR
displays opens as Grading instead of failing.

### Linux Pier

- Match My Layout on a desktop that another remote-desktop product owns now
  sets it to your monitors' own sizes on every connection, with one monitor
  or several, instead of showing black bars or an earlier connection's
  layout (to keep its layout, set `platform.session.guest_layout` to
  `"keep"`).
- Match My Layout and Primary Display Only now connect to a desktop whose
  monitor another tool scaled, instead of failing.
- A GNOME Wayland desktop that locks, after idle or by hand, keeps running
  with its applications open, and your next connection unlocks it and
  shows it again.
- A desktop the Pier started for you now locks as soon as your Deck
  disconnects, instead of staying open until its idle lock.
- The service's minute health record reads critical only when a session
  was, not for one brief dip as a stream starts or ends, and the log says
  at the default level why capture or encoding could not start.

### Windows Pier

- HDR on a PC whose displays have no HDR, such as dummy plugs, opens as
  Grading and the Deck says why, and HDR with Match My Layout on two
  displays opens too, instead of failing.
- A Grading or HDR display that a Deck leaves at 4:2:0 streams in 4:4:4
  instead of ending the session, and Match My Layout names any display it
  encodes in software, and why.
- The installer waits while the Pier service is still starting, so a slow
  start no longer fails the install, and the service starts sooner on a
  PC with many session logs.
- The logs record the session's end and the Pier's stop when Windows
  restarts or shuts down, and the service's minute health record shows how
  its session is doing instead of always reading unavailable.
- A Deck that drops leaves its session locked, and coming back asks for
  your password; with `privacy.desk = "required"`, the session locks before
  the PC's own monitors come back.

### macOS Pier (preview)

- With `privacy.desk = "required"`, a Mac whose session will not lock
  after your Deck leaves keeps its desk dark and its keyboard and mouse
  blocked until it locks, instead of giving it back unlocked after a minute.

### macOS Deck and Windows Deck

- After **Use Primary Display Only**, the session says it is on one display
  for this session only.
- On the Windows Deck, Stream Statistics shows the frame rate that reaches
  the screen (60 for a 60 fps stream, not 70), and a smooth stream no
  longer reads as degraded or critical in the session's health, on one
  display or several.
- On the macOS Deck, `arcen-deck multi-monitor-smoke --pipeline
  grading|hdr` streams every display in that preset, reports what the host
  serves and why, and names any display the host leaves out.

### Every Deck and Pier

- A frame-rate health warning in the log names the lowest rate that
  passes, so the rate that failed is always below it.

## [0.21.3] — 2026-11-25

Desk privacy is now a Deck setting, and a Pier keeps your session when the
desk cannot be made fully private instead of ending it. On a Windows Pier,
Match My Layout works from the sign-in screen.

### Windows Pier

- Match My Layout works when you sign in remotely at the sign-in or lock
  screen.
- When the PC's monitors cannot take your layout (fewer of them, or sizes
  they do not offer), Match My Layout opens on the monitors it has, each at
  its own size, instead of ending the session.
- With dummy plugs, or a monitor it cannot darken, the Pier now keeps your
  session, blocks the desk's keyboard and mouse, and tells the Deck that
  its screens may show the session, instead of refusing or disconnecting.
- Desk privacy follows each Deck's choice; `privacy.desk = "required"`
  keeps the desk private whatever the Deck asks, and refuses a session that
  cannot be.

### Linux Pier

- A joined desk that cannot be made fully private, such as a GNOME Wayland
  session shown on a desk monitor, now keeps your session and tells the
  Deck, during the session too.
- Desk privacy follows each Deck's choice, including a Deck that returns to
  a session; `privacy.desk = "required"` keeps the desk private whatever the
  Deck asks, and refuses a session that cannot be.

### macOS Pier (preview)

- A desk that cannot be made fully private no longer refuses or ends your
  session; the Deck is told instead.
- Desk privacy follows each Deck's choice; `privacy.desk = "required"`
  keeps the desk private whatever the Deck asks, and refuses a session that
  cannot be.

### macOS Deck and Windows Deck

- Settings › Privacy › Keep the host's desk private: on by default; turn it
  off to let people at the host see your session. It applies from the next
  connection.
- The Deck says when the host's screens could not be blanked or its input
  could not be blocked, and again when that changes during the session.

## [0.21.2] — 2026-11-24

The Linux Pier no longer mistakes a Bluetooth keyboard or a desk driver for
another remote-desktop product, and the Decks say what to do when one runs a
workstation's sign-in. ⌘+ and ⌘− now reach the host.

### Linux Pier

- On a workstation whose sign-in another remote-desktop product runs, a
  sign-in with nobody signed in now names that product and asks you to sign
  in with it first.
- On such a workstation, a desktop that does not lock when you leave stays
  private until it locks.
- Reinstalling after `--uninstall` forces the console outputs that
  `--console-outputs` chose again, so the console keeps its extra displays.
- Arcen no longer mistakes a Bluetooth keyboard, or the driver of a grading
  panel, mouse or key remapper at the desk, for another remote-desktop
  product, so it no longer refuses or ends a session because of one. The
  driver must run as a system service from its package.

### macOS Pier (preview)

- Control+arrow from a Deck switches Spaces on the Mac. Thanks, Taylor-VN.

### macOS Deck and Windows Deck

- When another remote-desktop product runs a workstation's sign-in, the
  Deck names it and says what to do.
- ⌘+ and ⌘− (Ctrl+ and Ctrl− on Windows) reach the host and no longer zoom
  the Deck's window. Thanks, Taylor-VN.
- Connecting names the saved connection and the username you typed.
- Test connection on the connection form shows its verdict beside the
  button and scrolls the report into view.
- In the smallest window, Test connection no longer runs under the
  connection form's Cancel button.
- Use Primary Display Only holds for that session only: the next
  connection asks the host for your whole layout again.
- Signing in at a host's sign-in screen no longer logs an error while the
  Deck moves to your desktop.

## [0.21.1] — 2026-11-24

A security release: every Pier should update, and every Deck too. It updates
the QUIC library (RUSTSEC-2026-0340 and others).

### Linux Pier

- A malformed connection attempt can no longer stop the Pier or exhaust its
  memory.

### Windows Pier

- A malformed connection attempt can no longer stop the Pier or exhaust its
  memory.

### macOS Pier (preview)

- A malformed connection attempt can no longer stop the Pier or exhaust its
  memory.
- Troubleshooting (`docs/operations/troubleshooting.md`) covers the macOS
  Pier: where each log is, and what to do about no sound in a first session,
  a rejected lock-screen password or a 30 fps stream.

### macOS Deck and Windows Deck

- A malformed reply from the network can no longer crash the Deck or exhaust
  its memory.

## [0.21.0] — 2026-11-24

Multi-monitor sessions run smoother on the macOS Pier (preview), and the Deck
draws the Mac's own pointer. On Windows, shortcuts another app claims reach
the host, and desk privacy rides out a brief display re-read.

### macOS Pier (preview)

- In Match My Layout, a busy display is no longer slowed by a still one:
  with two displays, the active one rose from about 38 to about 56 fps.
- The Deck draws the Mac's pointer in the Mac's own shapes, so it moves with
  your hand, hides when the Mac hides it and comes back when you move the
  mouse after typing.
- **Stop Sharing** in the macOS menu bar ends the session, tells the Deck
  "Sharing was stopped on the host." and locks the Mac; the Deck does not
  reconnect.
- If the Mac refuses to lock when a Deck leaves, the Pier asks again for up
  to a minute and reports an error if it never locks.

### Windows Pier

- With desk privacy on, a brief moment when the displays cannot be read, as
  when the NVIDIA Control Panel re-reads its connectors, no longer ends the
  session. A lit monitor still ends it at once.

### Windows Deck

- Alt+G and other shortcuts that another app on the PC claims now reach the
  host while system shortcuts go to the remote computer (the default).
  Ctrl+Alt+Del, Windows+L and characters typed with AltGr work as
  before.

## [0.20.8] — 2026-11-22

When a host turns a Deck away for a reason other than the password, the Deck
now says why, and a saved connection can keep its own system-shortcut
choice. Build 2, re-issued on 23 November, unlocks a Mac host's lock screen,
fixes desk privacy on the Windows Pier and keeps watched GNOME Wayland
desktops alive. Works with every Deck and Pier from 0.17.0.

### macOS Deck and Windows Deck

- A refusal that is not about the password says what happened in plain
  words, keeps the host's own words under **Details**, is not counted as a
  failed sign-in and is not retried.
- When someone else is signed in at the host, the Deck names that account
  and asks you to sign in as that user, or to switch to your account there
  first.
- New and Edit connection › **On this machine** › **System shortcuts go to
  the remote computer** offers **App default**, **On** or **Off** for each
  connection. On a Mac, a connection set to On still needs the
  Accessibility permission.
- Home sums up in one line what a connection keeps differently from
  Settings, with a **Use app defaults** link.
- New and Edit connection keep their buttons in view in a short window, and
  the session screens use the same buttons as Home and Settings.

### macOS Pier (preview)

- With Typing set to **Keys only**, capital letters reach apps that use
  secure input, such as Terminal's Secure Keyboard Entry. Option layers and
  Caps Lock are not covered yet.
- Choosing another account at the streamed lock screen ends the session
  with a message that names that account, not as a privacy failure.
- A restarted Pier locks the session its Deck was attached to.
- The session log counts the frames of a session that desk privacy ended.
- Known issue: on a Mac host's lock screen, a password with capital letters
  typed from a Deck may be rejected; signing in at the login window works.
  Fixed in build 2.

### Re-issued 2026-11-23 (build 2)

Every 0.20.8 download was rebuilt as build 2. The version stays 0.20.8, and
the first build stays available under its own tag.

#### macOS Pier (preview)

- A password typed from a Deck unlocks a Mac that has gone idle at its lock
  screen. The first key typed while the password field is hidden brings the
  field up; it and the keys after it arrive about 1.5 seconds late, in
  order.
- Known issue: a password typed straight into the lock screen can still be
  rejected when macOS has kept the hidden password field active, as after a
  Deck session ends and the Mac locks. Click the password field first, then
  type.

#### Windows Pier

- With desk privacy on, the Deck's mouse reaches the desktop and the pointer
  shows again; input from anywhere else is still blocked.
- With desk privacy on, moving content no longer freezes for about a second
  every two seconds. A desk monitor that lights up still ends the session at
  once.
- A single-display session gives the display's scale back when it ends.
- A busy host makes the Deck say *Another session is already using ‹host›*,
  not that the connection dropped.

#### Linux Pier

- On GNOME Wayland, a desktop a Deck is watching no longer locks and ends
  after five idle minutes; locking by hand still works. A desktop with no
  Deck attached can still lock and end when idle.
- On GNOME Wayland, resizing a Windowed Deck no longer ends the desktop. A
  size the host cannot serve keeps the old size, and the Deck says why.
- A failed attachment no longer ends a desktop the host created; the next
  sign-in rejoins it.
- A busy host makes the Deck say *Another session is already using ‹host›*,
  not that the connection dropped.

#### macOS Deck

- In a window, the picture sits below the toolbar and clicks land where the
  pointer is.

#### Every Deck and Pier

- A still desktop no longer reads as a Critical session. A presenter or host
  that stops, or video that falls behind moving content, is still reported.

## [0.20.7] — 2026-11-22

Both Decks have a calm new Home that follows your system's light or dark
appearance. A connection can be shared as a file and can keep its own
typing, keys and displays. Works with every Deck and Pier from 0.17.0.

### macOS Deck and Windows Deck

- Home lists your connections on the left and the selected machine on the
  right, with one **Connect**. The Deck turns light or dark with your
  system, and the user guide lists every menu and shortcut.
- A sign-in problem shows under its field in plain words, and you stay on
  your connections. Only an ended session or a changed host identity opens a
  box, and a changed identity shows both fingerprints before you trust it.
- **Export…** and **Import…** share a connection as an `.arcen` file. The
  file never holds a password, and an identity in it is trusted only when
  you tick it.
- A saved connection can keep its own Typing, Option key, ⌘ key and
  Displays; Settings hold the rest, with Tablet on a page of its own. The
  first start moves your settings over once: the tablet mode starts on
  **Tablet support** for every connection, and the Option keys go back to
  **Automatic**.
- The cursor is automatic: the Deck draws the host's pointer in the host's
  shapes, and shows none when the host draws it into the video.
- Also: **Low** certificate checking is offered only when the Deck starts
  with `ARCEN_ACCEPT_INSECURE=1`, and a saved Low without it checks as
  Medium.

### Windows Deck

- An idle Deck no longer keeps a CPU core busy.
- A session that ends while a menu is open no longer freezes the window; the
  Deck reconnects, or returns to your connections and says why.

### macOS Pier (preview)

- The Pier draws the pointer into the video for a 0.20.7 Deck, which shows
  no pointer of its own.

### Known limitations

- The macOS Pier's automatic pointer is not yet qualified on Mac hardware,
  and the Windows Deck's dark appearance has not yet had the Mac Deck's
  visual review.

## [0.20.6] — 2026-11-20

A Windows Pier on a GeForce serves the display it has, and its installer
protects a custom install folder. Works with every Deck and Pier from
0.17.0.

### Windows Pier

- On a GeForce, the session starts on the attached display at the mode
  nearest the Deck's size, and the Deck says why when it differs. An exact
  display size needs a Quadro, RTX Pro or GRID card. A GeForce with no
  display is refused: add a monitor, a dummy plug or a virtual display
  driver. The GeForce path has not yet run on a GeForce.
- If your host showed *exact display apply failed … NVAPI_NOT_SUPPORTED*,
  upgrade and connect once; leave any `display-recovery.unrestorable.*.json`
  files in `C:\ProgramData\Arcen\runtime`. A PC whose only display is a
  dummy plug also needs `"privacy": {"desk": "off"}` in `pier.json`, or the
  virtual display driver of the
  [GeForce guide](docs/operations/windows-geforce-virtual-display.md).
- The Pier leaves a GeForce display's Windows scale alone, and serves HDR
  only when Windows shows HDR on that display.
- The installer refuses a custom install folder that another user could
  rename or replace, and one on a network share, a mapped or `subst` drive
  or behind a link. The default folders and a data drive's root are not
  affected.
- Host audio from an output device at another rate than 48 kHz no longer
  carries audible artefacts. Not yet proven on such a device.

### Windows Deck

- Help ▸ About and Help ▸ Keyboard Shortcuts close during a session, and the
  key that closes them does not reach the remote computer. Not yet tried on a
  physical keyboard.

## [0.20.5] — 2026-11-20

A Linux Pier joins your own GNOME Wayland session, a macOS Pier plays host
audio at the right speed on any output, and renewing a certificate can no
longer leave a broken pair behind. Works with every Deck and Pier from
0.17.0.

### Linux Pier

- Joins your own GNOME session on Wayland at the workstation: it unlocks it
  after your password, locks it when the Deck leaves and never ends it. A
  Deck that disconnects signs in again. With desk privacy on, a session
  shown on a desk monitor is refused, and so is a Wayland login screen with
  nobody signed in. Not yet proven with the GDM login screen, a remote-first
  sign-in or a monitor plugged in mid-session.
- On a desktop it joins, keyboard, pointer and pen reach that desktop only.
  If that cannot be proved for five seconds, the Deck is disconnected with
  the reason and the workstation locks. An input remapper such as `keyd` or
  `input-remapper` prevents it. Ctrl+Alt+F1 to F12 from the Deck no longer
  switch sessions.
- Native tablet works on a joined Xorg desktop, yours or another
  remote-desktop product's. Not yet tested with a physical tablet.
- A host without a display manager never starts a second session for an
  account that has one: a session it cannot read, or an X11 session at the
  workstation, is refused. A slow logind no longer ends an attachment; after
  10 to 25 seconds without an answer, the workstation is handed back locked.
- `arcen-new-host-cert` no longer needs OpenSSL or `flock`, and writes to
  `/etc/arcen` (or `ARCEN_TLS_DIRECTORY`) instead of `/etc/arcen/tls`.

### Windows Pier

- The installer refuses a TLS or Arcen folder that is a link or belongs to
  another account (delete it and run again), and keeps the key readable
  only by SYSTEM and Administrators while it is written.
- `new-host-cert.ps1` now runs `install-arcen-pier.exe --tls-only`, which
  also takes `--renew`; `host.cert-sha256` and `host.spki-sha256` replace
  the old `.txt` files.

### macOS Pier (preview)

- Host audio plays at the right speed from an output device at any rate
  from 8 to 384 kHz, and follows a device that changes rate mid-session.
  Outside that range the session carries on without host audio and says
  why.
- Takes a renewed certificate without a restart:
  `sudo launchctl kill HUP system/pier.arcen.tech.service`. A bad
  replacement is refused, and the Pier keeps the certificate it had.

### Every Pier

- Renewing a certificate keeps every name the old one covered and keeps the
  key, so paired Decks still trust the host. An interrupted renewal is
  finished or undone by the next run, and keys written by `openssl` renew on
  a Mac and in the Windows installer too.
- Canvas screen updates no longer restart near the edge of a screen such as
  1920 × 1080.
- Support bundles refuse a file reached through a link, a second name or a
  swap.

### macOS Deck

- Host audio follows your Mac's sound output when you pick another output or
  unplug a device. Physically unplugging a device is not yet qualified.

### Windows Deck

- A Deck running inside a Windows Pier's session no longer makes that Pier
  end it, and the Windows key, Alt+Tab and Print Screen go to the host
  again. A shortcut that another program on your PC blocks is not sent. Not
  yet tried on a physical keyboard.

## [0.20.4] — 2026-11-20

One keyboard switch on both Decks, with a chord you choose, and slow, fine
scrolling reaches the host. A Windows Pier locks the session even when its
service dies. Works with every Deck and Pier from 0.17.0.

### macOS Deck and Windows Deck

- Ctrl+Alt+Home (⌃⌥Home on a Mac, Fn+⌃⌥← on a laptop) moves the keyboard
  between the remote computer and this one, and a short notice says which
  has it. A click into the picture gives it back to the remote and is not
  sent.
- **Settings › Input › Keyboard switch** takes another chord:
  Ctrl+Alt+Shift+Z, Alt+F1 or one you type. Chords the system never passes
  on are refused.
- Slow mouse-wheel and gentle trackpad or touchpad scrolling reach the host.

### macOS Deck

- **Settings › Input › Send ⌘Tab, ⌘Space and Mission Control to the remote
  computer** (off by default; needs the Accessibility permission) also sends
  ⌃← and ⌃→. ⌥⌘Esc, ⌃⌘Q and keys under secure input stay on the Mac. The
  first time ⌘Tab leaves a session, the Deck offers it once.

### Windows Pier

- If the Pier service dies while a Deck is attached, the session still
  locks once the desk has its monitors back; applications keep running. The
  desk's speakers stay silent until the service runs again.
- A wide-colour display without HDR starts an ordinary session at once, not
  after about 25 seconds.

### Linux Pier

- A Pier without a working virtual tablet no longer claims pen pressure,
  tilt or eraser; mouse and keyboard still work.

## [0.20.3] — 2026-11-18

Speed no longer tears, the Windows Deck keeps 60 frames a second on two
monitors, and every streaming preset explains itself. The re-issue of 19
November turns desk privacy on by default and gives the keyboard switch to
both Decks. Works with every Deck and Pier from 0.17.0.

### macOS Deck and Windows Deck

- Speed waits for the display's refresh, about 3 ms on average, so no preset
  tears.
- Each streaming preset says in Settings what it favours and what it gives
  up first on a weak link.

### Windows Deck

- With Match my layout on two monitors, each shows 60 frames a second.
  Drivers that cannot share pictures between devices work as before, and
  Stream Statistics says which way the Deck decodes.

### Linux Pier

- Fullscreen video read straight from the screen (Grading on a depth-30
  desktop, or software encoding) no longer arrives torn.

### Re-issued 2026-11-19

Every 0.20.3 download was re-issued on 19 November 2026 with these changes.

#### Every Pier

- **Desk privacy is on by default.** While a Deck is attached, the host's own
  monitors are dark and its keyboard and mouse are blocked; leaving locks the
  session, then gives the desk back. A desk that cannot be kept private
  refuses the session.
- **Upgrade note:** to keep the desk live, set `"privacy": {"desk": "off"}`
  in `pier.json`. On macOS, `platform.desktop.deskside.enabled: false` no
  longer turns privacy off, and `input_backend: virtual_hid` refuses
  signed-in sessions until it is `auto` or `coregraphics`, or privacy is
  off.
- F13 to F24 and the Menu key reach the host; a Mac has no F21 to F24,
  Insert, Print Screen, Scroll Lock or Pause. Pen barrel buttons right-click
  and middle-click on every host.
- Software encoding follows the Deck up to 3840 × 2160 at 60 fps, as fast as
  the CPU allows, and lowers quality instead of freezing motion.
- Canvas screen updates: a Linux or Windows Pier encoding in software sends a
  single-display Deck only what changed, with text kept exact, within a CPU
  budget (`video.software.cpu_share`, half the cores by default). **Settings
  › Streaming › Canvas screen updates** on either Deck turns it off.

#### Windows Pier

- With desk privacy on, the session runs on a virtual display while the
  desk's monitors are detached, and their layout comes back exactly. For a
  few seconds after sign-in the desk can still see the session, and pen
  tablets and touch screens at the desk are not blocked yet.
- Hardware encoding on Intel, AMD and other GPUs without NVENC, marked
  **Alpha**: Auto tries NVENC, then this encoder, then software. Grading,
  HDR and multi-monitor sessions do not use it. Turn it off with
  `platform.media_foundation_hardware: false`.

#### Linux Pier

- A Deck's keys never reach a text console, and typed text never presses
  SysRq.
- Grading or HDR with Match my layout on Wayland starts as Auto instead of
  failing.

#### macOS Deck and Windows Deck

- **Upgrade note:** F9, Ctrl+Win+K, Ctrl+Win+U and Ctrl+Alt+Win+V (⌃⌘K, ⌃⌘U
  and ⌃⌥⌘V on a Mac) now reach the host. Their actions are in the
  **Connection** menu: **Release Stuck Keys**, **Keyboard: Physical Keys /
  Characters** and **Paste as Keystrokes**.
- **Connection › Send Key** sends Print Screen, Scroll Lock, Pause, Insert,
  Ctrl+Alt+Del, Menu and F13 to F24; **Help › Keyboard Shortcuts** lists the
  keys the Deck keeps.
- **Settings › Input › Tablet support** sets each pen button to Right click,
  Middle click or Off, and the tablet driver's own clicks work while the pen
  hovers. Updating the Deck alone fixes a Linux host.
- Home shows which Displays choice a saved connection uses, with **Use
  Settings** to hand it back.

#### Windows Deck

- **Ctrl+Alt+Home** moves the keyboard to this PC and back, and **Settings ›
  Input › System shortcuts go to** chooses the remote computer (the
  default), fullscreen only, or this computer. Not yet proven on a physical
  keyboard.

#### macOS Deck

- F13, F14 and F15 send Print Screen, Scroll Lock and Pause to a Windows or
  Linux host.

## [0.20.2] — 2026-11-17

A Linux Pier works beside another remote-desktop product on the same
workstation, finds the monitors of the desktop it joins and says why a
sign-in failed. Match my layout works with a Deck monitor turned on its
side. Two re-issues followed, and the second locks the session on every Pier
when the Deck leaves. Works with every Deck and Pier from 0.17.0.

### Linux Pier

- Joins a desktop that another remote-desktop product runs, as a guest, and
  never changes it. While that product's own client is connected, the Deck
  is told so instead.
- Reads the desktop's monitors at every sign-in, so Match my layout works
  after the login screen restarts. On a GPU that drives no monitor, the
  installer recommends `--console-outputs` with the GPU's head names
  ([administration guide](docs/operations/pier-administration.md#joining-the-users-session-console-mode)).
- When another product is the display manager and is stopped, the Pier
  starts your own desktop session.
- `platform.session.default_desktop` (for example `gnome-classic`) names the
  desktop an account gets when it has never chosen one.
- GNOME no longer asks for an administrator password to create a colour
  device, or for Power Off at the desk, and every failed sign-in tells the
  Deck why.

### macOS Deck and Windows Deck

- Match my layout works with a monitor turned on its side; a host that
  cannot rotate a display gets it upright.

### Re-issue — Fixed

The 0.20.2 Decks and the Linux Pier were re-issued with these fixes; the
Windows Pier is unchanged.

#### Linux Pier

- A firmware hotkey device no longer makes the Pier think another remote
  product is in use.
- When the Deck leaves a joined desktop, the desk gets its sound back and
  Arcen leaves; a lost connection is held only for the reconnect window.
- Match my layout on another product's desktop shows each host monitor on
  the Deck display in the same place, scaled to fit when sizes differ.

#### macOS Deck and Windows Deck

- A drag in Match my layout crosses from one display to the next.
- Multi-display sessions no longer report critical health.

#### macOS Deck

- In Match my layout, a still display no longer goes dark or keeps a stale
  picture, and mouse and keys work on every display without stuck buttons.

### Re-issued 2026-11-18 evening

Every 0.20.2 download was re-issued on the evening of 18 November 2026 with
these changes.

#### Every Pier

- Video bitrate no longer collapses on a network path whose round trip
  drifts; real congestion is cut as fast as before.
- Auto serves each monitor at its own display's refresh, and 120 and 144 Hz
  Deck displays above 60 fps where the host can.
- A Pier names a display it cannot keep up with, and both Decks show it in
  Stream Statistics. An idle multi-monitor desktop no longer reads as
  critical.
- A trackpad pinch zooms on Linux and Windows hosts.
- The unused WSS listener (`--wss-port`) is gone.

#### Linux Pier

- A remote-first sign-in unlocks the login keyring, and the login screen
  comes back after a session ends on GDM 40.
- A session that will not lock as the Deck leaves stays blank, with the
  desk's keyboard and mouse off, until it locks (at most a minute).
- Match my layout on a console reaches beyond 8192 pixels, such as two 5K
  monitors side by side.
- Primary Display Only on another product's desktop shows one monitor, and
  Native tablet on the console desktop is refused up front with the reason;
  Tablet support works there.
- Applications keep the user's GPU and driver choices, and windows a layout
  moved go back.

#### Windows Pier

- Locks the session when the Deck leaves, then gives the desk its sound
  back. A session held for a Deck's return stays unlocked for now.
- Mutes the host's own speakers during a session; set
  `"local_playback": "audible"` under `audio` to keep them playing.
- Gives every display the Remote UI scale you chose, at the nearest step
  Windows offers, and the Deck names a display at another scale.
- A fresh sign-in of the same account takes over its own waiting session.
- `install-arcen-pier --uninstall --purge` keeps the Windows Deck's USB
  component state.

#### macOS Pier (preview)

- Locks the session when the Deck leaves, even if the Pier's session helper
  dies; a returning Deck signs in at the streamed lock screen.
- Desk privacy (`platform.desktop.deskside.enabled`, off by default) darkens
  the desk's displays and blocks its input while a Deck is attached. It does
  not cover keys typed into a password field, Siri, Voice Control,
  dictation, or displays fed by DisplayLink, Sidecar or AirPlay.
- Keeps the Mac awake while serving, wakes its displays when a Deck
  connects, and needs no restart after installing.
- Counts failed sign-ins once for the whole Mac, and a pen leaving the
  tablet reaches browsers.

#### macOS Deck and Windows Deck

- Auto and Speed skip ahead to the next keyframe when this computer cannot
  decode every frame; Grading and HDR still show every frame.
- Security mode High says why it refuses a host and what to do.
- Settings › Displays names a saved connection's own choice, and the Deck
  says when a host kept its own layout.
- The Deck names an older copy of itself that may open instead.

#### macOS Deck

- Auto shows nearly every frame on a scaled or windowed display, and Grading
  after signing in through the Deck stays ten-bit.

#### Windows Deck

- Alt+Tab, the Windows key, Alt+Esc and Ctrl+Esc go to the host whenever a
  session window has focus; Windows+L and Ctrl+Alt+Del stay with the PC.
- Match my layout no longer holds the decoder up. On some GPUs, two windows
  on two displays still decode about 60 frames a second between them.

## [0.20.1] — 2026-11-17

**Arcen is an extension of your machine.** A Linux Pier joins the session
you already have at the workstation, with your applications open. Signing in
from a Deck while nobody is at the desk goes through the workstation's own
login screen, and disconnecting locks the session; it never ends it. Works
with every Deck and Pier from 0.17.0; 0.20.0 ones work without the new
features.

### Upgrade notes

- **An upgraded Linux Pier with an Xorg server joins the console.** To keep
  a private desktop per sign-in, set `"mode": "dedicated"` under
  `platform.session` in `pier.json` and restart the Pier
  ([administration guide](docs/operations/pier-administration.md#joining-the-users-session-console-mode)).
- **GNOME on Wayland at the desk** cannot be joined yet: set
  `WaylandEnable=false` in `/etc/gdm/custom.conf` and restart GDM while
  nobody is signed in.
- **A host without monitors** needs `install-arcen-pier --console-outputs 4`
  to serve several displays; then restart the display manager while nobody
  is signed in, or reboot, and restart the Pier.
- Hosts installed from an older template keep `"fps": 60` under `video`,
  which caps Auto at 60; remove it to let Auto follow the Deck.

### Linux Pier

- Joins the session the account holds at the workstation. With nobody
  signed in, the login screen signs the account in, so a later sign-in at
  the desk resumes the same session. While a Deck is attached, the desk's
  monitors are blank and its keyboard and mouse are off.
- Another account at the workstation, or another remote-desktop service
  holding it, is refused with a message that names it.
- A host without a display manager keeps one login session per account,
  which survives a Pier restart and stays blank and private between
  sign-ins.
- Active Directory accounts (SSSD with Group Policy) sign in remotely; the
  installer allows Arcen's sign-in services to log on locally, nothing
  broader.
- Match my layout gives one stream and one window per monitor. The
  workstation takes the Deck's layout while it is attached and gets its own
  back exactly when it leaves; a console that cannot take it offers Primary
  Display Only.

### Every Deck and Pier

- The monitors of a session show the same moment of the desktop, with any
  Pier.
- Auto follows your display's refresh, up to 60 Hz, instead of a fixed
  30 fps, and Stream Statistics says what limits it.
- Both Decks report a rotated monitor, and the macOS Pier accepts one.

### Known limits

- The macOS Pier remains a preview; its 0.20.1 changes have not yet run on a
  real Mac. Restart the Mac after installing or upgrading it.
- GNOME on Wayland at the desk is refused, and the headless Wayland desktop
  still starts a new desktop per connection.
- Decks ask for remote displays no faster than 60 Hz, so a 120 or 144 Hz
  display is served at 60, and four 4K monitors can run below 60 fps each on
  some GPUs.
- Console monitors are found when the Pier starts: after restarting the
  display manager or changing `--console-outputs`, restart the Pier.
- After a sign-in from a Deck with nobody at the desk, the login keyring
  stays locked, and GNOME may ask for an administrator password to "create a
  color managed device"; choose Cancel.
- A host without a display manager has no lock screen: its session resumes
  only through Arcen.
- The 0.20.0 limits for pens with Windows Ink turned off and for the
  Windows Deck with a Windows Pier still apply.

## [0.20.0] — 2026-11-16

**The Windows Deck.** Arcen Deck now runs on Windows 11 x64 as well as on
the Mac, with the same screens, presets, sign-in, reconnect and Stream
Statistics. It connects to Linux and macOS Piers with keyboard, mouse, pen,
clipboard both ways, host audio and microphone. Works with every Deck and
Pier from 0.17.0; 0.19.0 ones work without the new features.

### Windows Deck

- Two MSIs: the Deck installs for all users and never runs elevated; the
  optional USB component is needed only for Native tablet and lends the
  tablet Windows' own WinUSB driver for a session. There is no Arcen kernel
  driver ([packaging](packaging/windows-deck/README.md)).
- The MSIs are unsigned. SmartScreen says "Windows protected your PC":
  choose **More info**, then **Run anyway**. Smart App Control in
  enforcement mode blocks them; turn it off to install Arcen.
- Auto and Speed decode H.264 and HEVC on the GPU. Grading and HDR need the
  GPU's HEVC 4:4:4 ten-bit decoder; without it, Settings greys them out and
  names what is missing.
- Tablet support takes the pen from Windows Ink, with pressure, tilt, barrel
  and eraser; Native tablet attaches any USB pen tablet to the host.
  Ctrl+Alt+End sends Ctrl+Alt+Del, and the Deck reconnects by itself after a
  network loss.
- Displays work as on the Mac: Match my layout for up to four displays,
  Primary display only, or Windowed. Fullscreen draws nothing over the
  video; the connection bar opens at the top edge or with Ctrl+Alt+Home.
  Settings live in `%APPDATA%\Arcen\Deck` and logs in
  `%LOCALAPPDATA%\Arcen\Deck`; no password is stored.

### macOS Deck and Windows Deck

- Each preset shows frames on its own terms: Auto about twice as soon as
  before without tearing, Speed sooner still, Grading and HDR whole at the
  refresh. Fullscreen is pixel for pixel.
- A Deck never asks for video its decoder cannot decode, and Settings names
  the GPU and what it lacks. A host that cannot serve the requested video
  says so after the password, and it does not count as a failed sign-in.
- Support bundles replace user names, host names, addresses and home-folder
  names with codes, and leave crash reports out unless you include them:
  Settings › Logs › Create support bundle….
- Tablet support follows the tablet driver's own handedness and mapping;
  View › Tablet Orientation is gone.
- A connection an older Deck saved as WSS opens over QUIC, on port 18444
  instead of 18443.

### Every Pier

- Tells the Deck the host's keyboard layout, so every character types right
  from the first key.
- Slows down repeated failed sign-ins per source and account, and admits at
  most sixteen waiting connections, two per address; a person has 60 s to
  type a password.
- Pen pressure arrives exactly as the Deck sent it, and Native tablet takes
  any USB pen tablet.
- Updates rustls to 0.23.46 with upstream's security fixes.

### Windows Pier

- Native tablet works with no driver: the pen arrives as a Windows Ink pen,
  with pressure, tilt, barrel button and eraser.
- The host's displays and windows come back when a session ends; an
  interrupted session keeps its display until the Deck returns.

### Known limits

- macOS Pier: browsers get no `pointerleave` when the pen lifts out of
  range.
- Windows Deck: a tablet whose driver has Windows Ink turned off arrives as
  a mouse; turn Windows Ink on, or use Native tablet.
- macOS Deck: Grading after a macOS host's sign-in screen can show in eight
  bits.
- A Linux Pier that is killed may leave the private desktop running until
  the next sign-in. Not yet tested.
- macOS Pier: restart the Mac after installing or upgrading it, or it may
  refuse every session at the login window; a few failures after sign-in
  still close the connection without a reason.
- The Windows Deck works with a Windows Pier, but that pairing is not yet
  qualified.
- The macOS Pier is a preview: the cold-boot login window and multi-display
  hardware are not qualified.

## [0.19.1] — not published (included in 0.20.0)

**Exact still image, on by default.** When part of the picture stops moving,
the Pier sends its exact pixels and the macOS Deck checks them, so a still
picture becomes the host's own pixels
([user guide](docs/user-guide.md#exact-still-image)).

### Every Pier

- Works in single-display Grading and HDR sessions from Linux X11 at depth
  30, Linux Wayland on GNOME 50 or later, Windows and macOS. Other sessions
  stream as before, and Stream Statistics says why it is off.
- Uses only spare bandwidth while the picture is still, and pauses whenever
  video waits. On a local network, a 1080p desktop is exact in under a
  second, a 4K photograph in about 6.5 seconds.
- `video.lossless_refinement` defaults to `enabled`; set `"disabled"` to
  turn it off.

### macOS Deck

- **Settings › Streaming › Exact still image** is on by default, and Stream
  Statistics shows *Exact: 100% of picture verified* once every pixel is
  checked. A macOS host needs a Deck of this release.

## [0.19.0] — 2026-11-12

**Native tablet ships.** The pen tablet on your Mac's desk attaches to the
remote desktop as if it were plugged into the host, proven with a real Wacom
Intuos5 on Linux X11, Linux Wayland and macOS Piers. The Deck's pen dot
follows the pen about 16 ms behind it, against 80 ms before. The macOS Pier
types text, and the licence texts are final. Works with every Deck and Pier
from 0.17.0; pen barrel buttons need an updated Deck and an updated macOS or
Windows Pier together.

### Coming from 0.17.0

0.18.0 was not published, so 0.19.0 also brings its changes (listed under
[0.18.0] below): Paste as Keystrokes and Characters typing, Mac keyboards on
Windows hosts the Windows App way, real cursor images on X11 and Windows
hosts, the Colour check, **Test connection** and `arcen-deck diagnose`.
Wayland hosts re-qualify on their first start (a few minutes), and a Windows
user's keyboard layout should match the Mac keyboard.

### Upgrade, step by step

1. **Deck:** quit it, replace **Arcen Deck** (`Arcen-Deck-0.19.0-macOS.zip`,
   Apple silicon) or **Arcen Deck Intel** (`Arcen-Deck-Intel-0.19.0-macOS.zip`)
   in Applications, and open it. With Native tablet, restart the Mac (or turn
   the Arcen USB helper off and on in System Settings › General › Login
   Items).
2. **Linux Pier,** in a maintenance window (a restart ends running sessions):
   ```sh
   chmod +x install-arcen-pier-0.19.0-linux-x86_64          # a download is not executable
   sudo ./install-arcen-pier-0.19.0-linux-x86_64 --check     # what it will do
   sudo ./install-arcen-pier-0.19.0-linux-x86_64 --restart
   ```
   The upgrade keeps `/etc/arcen/pier.json`. Add `--native-tablet` to turn
   Native tablet on, `--no-native-tablet` to turn it off.
3. **Linux Pier that used `ARCEN_USB_HARD_LAB`:** run step 2 with
   `--native-tablet`, delete the systemd drop-in that sets it
   (`systemctl cat arcen-pier` names it) and run
   `sudo systemctl daemon-reload`.
4. **Linux X11 host with Native tablet for the first time:** restart the
   display manager once while nobody is signed in at the console
   (`sudo systemctl restart gdm`), or reboot.
5. **Windows Pier:** run `install-arcen-pier-0.19.0-windows-x64.exe` in
   PowerShell as Administrator, then reboot.
6. **macOS Pier (preview):** open `ArcenPier-0.19.0.pkg` in Installer.app;
   existing approvals carry over.

### Linux Pier

- Native tablet is an administrator opt-in: `install-arcen-pier
  --native-tablet` turns it on (on Rocky Linux and RHEL it also installs
  `kernel-modules-internal`). See
  [Native tablet](docs/operations/pier-administration.md#native-tablet-usbnative_tablet).
- On X11 the tablet works from the first connection, with its own driver's
  pressure curves and side switches. On GNOME Wayland it works on
  native-seat desktops; headless desktops refuse it.
- A host copy reaches the Deck once, and a desktop ended outside Arcen gives
  its seat back at once.
- Grading keeps the desktop's own colours for a 0.18.0 or later Deck,
  including the dark greys of a Wayland desktop.

### macOS Pier (preview)

- Native tablet for the Wacom Intuos5 touch L, with pressure, tilt, eraser
  and barrel buttons. Other models are refused, and the Deck keeps its
  tablet; touch and ExpressKeys do not reach the host.
- Types text: input-method text, Characters typing and Paste as Keystrokes
  arrive as typed, whatever the Mac's layout.
- Pen pressure reaches applications, and Connection Health shows the Pier's
  delivery and network path.

### Windows Pier

- Pens arrive through Windows Ink only: Wintab is not supported, and Native
  tablet is not offered on Windows hosts.
- The eraser, the second barrel button and a pen on a display left of the
  primary now work.
- A Deck that crashed hands its session to the same user's next sign-in at
  once.
- A refused Grading or HDR session names its hardware cause
  ([error codes](docs/operations/error-codes.md#grading-and-hdr-refusals)).
- On a slow network, pictures stay fresher and stutter far less;
  `ARCEN_VIDEO_BACKLOG=legacy` restores the old behaviour.

### macOS Deck

- Captures the tablet for a session and gives it back to the Mac as if it
  were replugged, and hides the Mac's arrow over the video while you draw.
- The Option keys have a new default, **Automatic**: Option for a Mac host,
  left Alt and right characters for Windows and Linux hosts.
- A [user guide](docs/user-guide.md) for artists and editors, and **Stream
  Statistics** in the Session menu.
- The USB helper moved to a safer socket: restart the Mac after updating the
  Deck, or turn the helper off and on in Login Items.

### Legal

- The licence exception, CLA, trademark policy and notice are final
  (version 1.0, effective 2026-11-12), and the exception names the Microsoft
  Azure Marketplace beside the Apple and Microsoft stores. Official builds
  carry a published build ID
  ([how to check](docs/operations/versioning-and-releases.md#telling-an-official-build-apart)).

### Known limitations

- Ink in X11 applications trails the pen dot on a slow link.
- GNOME Wayland draws lighter strokes: raise the stylus pressure curve in
  GNOME Settings › Wacom Tablet. A tablet remapped there offsets the pen dot.
- A held upper side switch on X11 shows pressure but draws nothing; lift the
  pen out of range and back.
- Safari reports every pen as a mouse; test pen pages in Chrome or Firefox.
- Multi-monitor Wayland and Windows Piers do not report pen positions yet.
  Native tablet on an Intel Deck is untested and needs macOS 13.
- The macOS Pier is a preview: the cold-boot login window and multi-display
  hardware are not qualified.

## [0.18.0] — 2026-11-09

A Mac keyboard works on a Windows host the way Windows App makes it work,
and Windows and Linux hosts type text by character. The new Colour check
measures every decoded pixel, and **Test connection** checks the way to a
Pier without a password. 0.18.0 was not published on its own; its changes
reached users in 0.19.0. Works with every Deck and Pier from 0.17.0.

### Upgrade notes

- **Linux Pier:** a host whose `/tmp/.X11-unix` is owned by a user serves
  desktops without X11 applications until an administrator runs
  `arcen-pier repair-x11-unix-dir` and re-qualifies. Wayland hosts
  re-qualify on their first start (a few minutes).
- **Windows Pier:** keys arrive by position, so the Windows user's own
  layout decides each character. Set it to match the Mac keyboard (for
  example Norwegian for a Norwegian Mac keyboard).
- **Windows Grading** carries the desktop's own sRGB colours to a 0.18.0
  Deck; update the Deck to get them.
- `ARCEN_QUIC_CONGESTION` accepts `bbr` (the default), `cubic`, `newreno`
  and the experimental `bbr-guarded`. `ARCEN_VIDEO_BACKLOG=legacy` restores
  the Linux Pier's 0.17.0 video queue.

### macOS Deck

- **Connection › Paste as Keystrokes** (⌃⌥⌘V) types the Mac clipboard's
  text where clipboard sharing cannot reach, such as a sign-in or lock
  screen. It asks before more than 2,000 characters, Esc stops it, and the
  text is never logged.
- **Keyboard: Characters** (⌃⌘U) sends each character as typed on the Mac,
  whatever the host's layout; **Physical Keys** (⌃⌘K) sends key positions.
- On a Windows host, ⌘ is the Windows key, ⌘C and ⌘V are Ctrl shortcuts,
  right ⌥ is AltGr, and the ISO key beside left ⇧ is its own key. Each saved
  connection can choose another mapping.
- **Session › Colour Check…** compares what the host serves with what you
  asked for and measures every decoded pixel against a test pattern. Retina
  fullscreen on a notched Mac is 1:1 again.
- **Test connection** and `arcen-deck diagnose` check the way to a Pier
  step by step without a password, and Connection Health shows the Pier's
  own measurement of the network path.

### Linux Pier

- Xorg desktops type text by character. GNOME Wayland types only characters
  the session's layout proves, and refuses and counts the rest.
- With the Mac's own cursor, X11 desktops show the application's real
  cursor images.
- Grading from a Retina Mac is served at its full size, and a 5K display at
  the largest size that fits.
- Encoded video no longer piles up behind a slow network.
- A host idle for ten days no longer loses X11 applications.

### Windows Pier

- Types text by character, and non-US punctuation comes out right.
- With the Mac's own cursor, the Deck shows the application's real cursor
  images.
- Grading keeps the desktop's colours code for code for a 0.18.0 Deck.
- HDR at Retina size no longer ends the session, and an HDR mode Windows
  will not turn on is refused with a reason.
- A refused Grading or HDR session names its hardware cause.

### Documentation

- [Error codes](docs/operations/error-codes.md), the
  [configuration reference](docs/operations/config-reference.md) and
  [Troubleshooting](docs/operations/troubleshooting.md) list every code,
  every `pier.json` key and every symptom.

## [0.17.0] — 2026-10-21

The Linux Pier serves Wayland as well as X11, and it closes a hole that gave
a remote X11 desktop the console's administrative rights. Update Deck and
Pier together: 0.17.0 refuses a 0.16 Deck or Pier and says to update both.
Linux operators: read the upgrade notes first.

### Upgrade notes

- **Linux hosts are muted by default** during a session; set
  `audio.local_playback` to `audible` to keep the speakers playing. A user
  also signed in at an X11 console is silent there too.
- **X11 desktop depth is `auto`:** depth 30 only for Grading or HDR, so
  browsers keep the GPU. Grading hosts (Flame, Resolve) should set
  `video.x11_desktop_depth` to `30`.
- **One X11 desktop per host:** sign every user out of the old build's
  desktops before upgrading.
- **Wayland hosts re-qualify themselves** at start after an update; expect
  one re-qualification now. `capture.wayland_auto_requalify: false` refuses
  instead.
- **A polkit rule** (`/etc/polkit-1/rules.d/05-arcen-remote-session.rules`)
  answers first for remote desktops: renumber site rules for remote users
  `00` to `04`, and `arcen-pier polkit-rule status` lists the ones affected.
  On polkit older than 0.106 (such as Ubuntu 22.04) the rule cannot be
  installed, and X11 desktops are served with a `SECURITY:` error in the
  log.
- **Native Wayland desktops** (`wayland.session_mode`, default `auto`) run
  where qualification proves them: an NVIDIA GPU without display connectors
  and `vkms`. A display-manager restart freezes one; the installer offers
  `--disable-gdm`. Set `headless` to keep every desktop headless.
- The Pier needs `libpipewire-0.3.so.0` on every host, X11 included; the
  installer names a missing library. The optional
  `capture.wayland_compositor_prefix` runs a newer GNOME beside the
  system's, for Grading and HDR.

### Security

- Linux Pier (X11): a remote desktop no longer gets the console's polkit
  rights. In 0.16.1 and older, its user could power off or reboot the host,
  control its network, mount disks or set the clock without a password.
- Linux Pier (X11): the keyboard and mouse at the console no longer drive a
  remote desktop, and a second desktop no longer receives the first one's
  input.
- Linux Pier: Alt+PrintScreen can no longer run a kernel SysRq action, and
  the password is wiped on every refused sign-in.
- Linux Pier (Wayland): Native tablet is refused, because the tablet would
  have reached the machine's own seat.

### Linux Pier

- A host without Xorg gives each sign-in its own GNOME Wayland desktop:
  Speed where proven, Grading and HDR on GNOME 50 or later, one to four
  monitors, clipboard, audio, typed text and X11 applications. The Pier
  chooses Xorg or Wayland from what the host has.
- A native Wayland desktop on stock GNOME gives the Deck's pen real pressure
  and tilt.
- Speed on Xorg reaches 60 fps, a pen on X11 reaches applications as a pen
  only, and a sign-in by the same user takes over its own detached session.
- Desktops get the user's locale, and the host never suspends under one.
- Sessions on SELinux hosts (RHEL, Rocky, Alma, Fedora) no longer fail with
  `pam_systemd did not create the required logind session`.

### Windows Pier

- `platform.font_smoothing = "grayscale"` takes the colour fringes off text
  for a session.
- A session that starts after the display went to sleep streams at full
  rate at once.

### macOS Deck

- The pointer follows the mouse 13 to 15 ms behind it, not 77 to 96 ms.
- Text edges in 4:2:0 streams are no longer coloured in hard steps.
- The Deck shows why a Pier refused it, and menu commands, Quit included,
  apply at once.

### Every Deck and Pier

- A blocked video stream no longer holds up audio, input or control, and
  clipboard items of 1 MiB or more cross again.

### Known limitations

- Wayland pen: a real pen needs a native desktop (one per host, an NVIDIA
  GPU without connectors, `vkms`, polkit 0.106 or later), verified only on
  Rocky Linux 10.2 with GNOME 49. On a headless desktop the pen acts as a
  mouse, and Native tablet is refused on Wayland.
- Wayland Grading and HDR need GNOME 50 or later and one output; several
  outputs are served as Auto. A desktop opened for Auto or Speed cannot be
  reattached with Grading or HDR: sign out and reconnect.
- Wayland has no hardware encode on Intel or AMD, refuses characters outside
  the keyboard layout, and adds about 10 to 20 ms against X11.
- An Xorg Grading desktop runs at depth 30, where Firefox and Chrome render
  in software (about 8 fps). A login screen that becomes active on the
  console during a remote session can receive the remote user's input.
- Typed text and native cursor images are Wayland-only for now, and the
  Windows Pier does not mute its speakers yet.
- The macOS Pier stays a preview: cold-boot login-window operation,
  multi-display hardware and microphone capture are not qualified.

## [0.16.1] — 2026-10-17

Sessions start on a freshly installed Linux host again.

### Linux Pier

- A fresh install no longer fails with `dedicated Xorg failed to start`:
  the installer finds the NVIDIA GPU and sets it in `/etc/arcen/xorg.conf`,
  keeping a backup of an operator's file. Rerun the installer to repair an
  existing file.
- On a host with several GPUs, capture and encoding run on the GPU that
  shows the screen.

## [0.16.0] — 2026-10-16

The Deck runs on Intel Macs and on macOS 12, and pen support works with any
pen tablet, not only Wacom.

### macOS Deck

- Two apps for macOS 12 or later: **Arcen Deck** for Apple silicon and
  **Arcen Deck Intel** for Intel Macs. AV1 needs Apple silicon and macOS 14,
  and Native tablet needs macOS 13.
- Tablet support works with any pen tablet, USB or Bluetooth, attached
  before or after you connect.
- Keys work before the first click.

### Linux Pier

- Native tablet (not yet in release builds) bridges any pen tablet, its express keys
  included. A 0.15 Deck's Native tablet request is refused with a reason;
  Tablet support is unaffected.

### Every Deck and Pier

- Updates rustls to 0.23.45. 0.15 and 0.16 Decks and Piers work together.

## [0.15.0] — 2026-10-06

Auto, Speed, Grading and HDR become named pipelines with their own bitrate
ceilings, the macOS Deck shows Auto and Speed with far less CPU, and the
macOS Pier takes the Deck's microphone.

### Every Pier

- The Deck asks for Auto, Speed, Grading or HDR by name, and the host says
  which one it serves. Grading may climb to 250 Mbit/s and HDR to
  500 Mbit/s, and Speed is tuned for one frame of latency at 60 fps.
- A host with only software encoding serves Speed as Auto at 30 fps, and
  refuses Grading and HDR with a reason.
- 30 fps sessions on NVIDIA get their full bitrate, and packaged
  configurations turn QP maps on for Auto and Speed.

### macOS Pier (preview)

- Sends host audio again for signed-in sessions.
- Installs an **Arcen Microphone** input device that carries the Deck's
  microphone when `microphone_input.enabled` is true.
- Honours `redirection.timezone`: apps started in the session use the
  Deck's time zone until the session ends.
- Auto and Speed are served as such, not as `custom`.

### Windows Pier

- An idle desktop no longer costs about 4.8 Mbit/s.
- A display on a virtual display adapter, such as a GeForce with a virtual
  display driver, streams again at its nearest mode, and a GeForce is no
  longer asked to write an EDID.
- HDR turns on for an HDR session and off for the others, and the user's own
  setting comes back afterwards. Mixed landscape and portrait Deck layouts
  work.
- Every failure before the session starts reaches the Deck with a reason.

### Linux Pier

- An upgrade renews only Arcen's own self-signed certificate, keeping the
  key; an operator's or a CA's certificate is left alone.

### macOS Deck

- Auto and Speed frames go straight to the screen, and a still desktop no
  longer keeps the Mac busy (about 48 % CPU before).
- A Wi-Fi or VPN stall no longer starts a repeating keyframe loop.
- The last picture stays on screen through a reconnect or the sign-in
  hand-over.
- Shows a Local Network hint when a host on the LAN cannot be reached.

## [0.14.0] — 2026-09-28

Every host starts with the same defaults, everything on, and the bitrate
follows the network on every Pier. The macOS Pier installer says what it
installs and asks for its approvals one at a time.

### Every Pier

- Installers write the same defaults everywhere: ten-bit video, Opus audio,
  microphone, clipboard both ways and time-zone redirection. Turn things off
  in the file; existing configurations are kept.
- The bitrate follows the network, with BBR congestion control, and random
  loss on Wi-Fi or a VPN no longer counts as congestion.
  `ARCEN_RATE_CONTROL=0` restores the fixed rate, and
  `ARCEN_QUIC_CONGESTION=cubic` restores Cubic.
- A preset the encoder cannot sustain starts at the best proven rate and
  says so, instead of being refused.
- Trackpad scrolling is precise, and gestures reach a macOS host.
- An install succeeds only when the Pier is running.

### Windows Pier

- Multi-display works on any eligible NVIDIA adapter, with no install-time
  probe; `excluded_adapters` keeps a GPU for other work.
- Colour conversion runs on the GPU in every preset.
- The same user reconnecting takes over the session at once, and time-zone
  redirection works.

### Linux Pier

- Multi-monitor finds the NVIDIA heads by itself;
  `platform.multi_monitor.heads` overrides it.
- After an upgrade without `--restart`, the running Pier keeps serving.
- On a `rec2100-pq` desktop, Grading is true BT.709 SDR and eight-bit
  presets are refused.

### macOS Pier (preview)

- One virtual display per Deck display, arranged as on the Deck.
- The installer says what it installs, shows one app in Applications and
  asks for system audio, Accessibility and Screen Recording one at a time;
  `uninstall.sh --purge` resets those approvals.
- No audio is dropped on a calm network.

### macOS Deck

- With several displays, the pointer no longer bounces between them.
- **Forget and Verify** reconnects at once, and a host's reason for closing a
  session is shown.
- Known issue: with several displays the Deck redraws far more often than
  frames arrive, which costs CPU.

### Known limitations

- macOS Pier: no microphone input or desk privacy yet, and the login-window
  agent runs as root.
- Windows Pier: the installer is unsigned, the optional microphone driver is
  not included, and a fresh install needs one reboot before remote sign-in.
- Linux Pier: needs an NVIDIA GPU and the installer's depth-30 Xorg
  configuration; HDR works only on a `rec2100-pq` desktop.

## [0.13.0] — 2026-09-27

The first macOS Pier package, HDR on Linux, and hosts that build their
display from the Deck's.

### macOS Pier (preview)

- A signed, notarised package installs a working host. Privacy permissions
  are approved at the Mac, so the command-line `installer` is refused by
  default.
- The login window streams, and the Deck follows sign-in into the desktop.
- Speed, Grading and HDR are separate pipelines; HDR runs only on proven EDR
  headroom, with SDR white at 203 nits.
- Still a preview: no cold-boot login window, and multi-monitor is not
  hardware-qualified.

### Linux Pier

- `video.desktop_encoding: "rec2100-pq"` declares a PQ desktop, such as
  Flame's HDR interface, so HDR is passed through as PQ; the default `sdr`
  serves Grading.
- The session's display is built from the Deck's, and audio has its own
  priority stream.

### Windows Pier

- HDR displays carry the Deck display's gamut and brightness, with SDR white
  at 203 nits, and HDR reaches its frame rate.
- An upgrade needs no flags, and a host without NVIDIA (a virtual machine)
  serves every preset as software H.264 instead of failing.

### macOS Deck

- Sends each screen's gamut and HDR headroom to the host, and greys out HDR
  when no screen can show it.
- ⌘ shortcuts reach a macOS host; hold ⌘Q for three seconds to quit the
  Deck.

### Known limitations

- macOS Pier: someone must have signed in once since boot, and capture at
  the login window needs a one-time approval at the Mac.
- Windows Pier: the installer is unsigned, the optional microphone driver is
  not included, and a fresh install needs one reboot before remote sign-in.
- Windows Pier on NVIDIA vGPU with multi-monitor: the virtual displays can
  disappear after a few sessions, and the Deck reports "authentication
  failed: DXGI could not uniquely resolve …"; reboot the host.
- Linux Pier: needs an NVIDIA GPU and the installer's depth-30 Xorg
  configuration, and an SDR session on a `rec2100-pq` desktop shows the PQ
  values as SDR.
- A completely still desktop may read as "critical"; the stream is fine.

## [0.12.0] — 2026-09-12

A native macOS Pier completes a session with the real Deck. It is not yet
complete: there is no installer, no cold-boot login window and no Native
tablet, and multi-monitor is not hardware-qualified.

### macOS Pier (in progress)

- Signs in through PAM, streams the desktop, takes pointer, keyboard and
  pen, carries the clipboard, and sends host audio while the Mac's own
  speakers stay silent.
- A rejected connection no longer takes the host offline, and an account is
  no longer served the console user's desktop.

### Every Pier

- `audio.local_playback`, default `muted`: someone beside the host never
  hears the remote user.

### Windows Pier

- Certificate renewal keeps the key, so pinned Decks keep trusting the host.

### Linux Pier

- `new-host-cert` writes the host's names and addresses into the
  certificate again.

## [0.10.0] — 2026-09-01

Four complete presets, Auto, Speed, Grading and HDR, replace separate
performance and colour controls, with real ten-bit and HDR paths.

### Every Deck and Pier

- Auto and Speed keep the fast eight-bit path, and Grading and HDR pay for
  their fidelity on their own. The Deck tells ten-bit SDR from real HDR and
  shows every fallback.

### Windows Pier

- Grading is genuine ten-bit, and HDR runs end to end on a session HDR
  display in HEVC 4:4:4 10-bit.
- A forced reconnect keeps the desktop without a new sign-in, and the
  installer works on localised systems.

### Linux Pier

- Grading on a depth-30 Xorg desktop is genuine ten-bit. An HDR request is
  served as Grading, BT.709 SDR, until a colour-managed Wayland route
  exists.

### macOS Deck

- Grading and HDR show through a ten-bit Metal layer, and EDR turns on only
  when the host sends PQ.

## [0.9.8] — 2026-08-25

**The first public release.** Arcen is published for the first time, as free
software under the AGPL-3.0. It is numbered 0.9.8, not 1.0.0, because
interfaces may still move and only two of the six host and client pairs
exist.

### Every Deck and Pier

- QUIC over TLS 1.3 on UDP 18444, with no TCP fallback.
- On first connection the Deck shows the host's fingerprints and lets you
  cancel, trust once, or trust and remember; pins are kept per saved
  connection.
- A dropped session reconnects within a bounded window and keeps the
  desktop.
- NVENC H.264 and HEVC up to 4:4:4 10-bit, AV1 and a software H.264
  fallback; Opus host audio, the microphone to the host, keyboard, mouse,
  pen pressure, USB tablet passthrough to Linux hosts and time-zone
  redirection.
- Every program names its licence and where its source is, and no feature is
  locked: the old licensing system is gone.

### Linux Pier

- A private Xorg desktop per sign-in, PAM sign-in and one self-contained
  installer.

### Windows Pier

- A virtual display and a Windows sign-in provider; a remote sign-in is
  refused while a local account holds the console.

### macOS Deck

- A native client with saved connections and trust for each of them.

### Known limitations

- Arcen is for direct connections on a trusted network, not for exposure to
  the internet.
- Released binaries are not code-signed on every platform; expect Gatekeeper
  and SmartScreen prompts.
- The macOS Pier, a Linux Deck and a Windows Deck do not exist yet.

[Unreleased]: https://github.com/Aanerud/arcen_public/compare/v0.21.4...HEAD
[0.21.4]: https://github.com/Aanerud/arcen_public/compare/v0.21.3...v0.21.4
[0.21.3]: https://github.com/Aanerud/arcen_public/compare/v0.21.2...v0.21.3
[0.21.2]: https://github.com/Aanerud/arcen_public/compare/v0.21.1...v0.21.2
[0.21.1]: https://github.com/Aanerud/arcen_public/compare/v0.21.0...v0.21.1
[0.21.0]: https://github.com/Aanerud/arcen_public/compare/v0.20.8-build2...v0.21.0
[0.20.8]: https://github.com/Aanerud/arcen_public/compare/v0.20.7...v0.20.8
[0.20.7]: https://github.com/Aanerud/arcen_public/compare/v0.20.6...v0.20.7
[0.20.6]: https://github.com/Aanerud/arcen_public/compare/v0.20.5...v0.20.6
[0.20.5]: https://github.com/Aanerud/arcen_public/compare/v0.20.4...v0.20.5
[0.20.4]: https://github.com/Aanerud/arcen_public/compare/v0.20.3...v0.20.4
[0.20.3]: https://github.com/Aanerud/arcen_public/compare/v0.20.2...v0.20.3
[0.20.2]: https://github.com/Aanerud/arcen_public/compare/v0.20.1...v0.20.2
[0.20.1]: https://github.com/Aanerud/arcen_public/compare/v0.20.0...v0.20.1
[0.20.0]: https://github.com/Aanerud/arcen_public/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/Aanerud/arcen_public/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/Aanerud/arcen_public/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.17.0
[0.16.1]: https://github.com/Aanerud/arcen_public/releases/tag/v0.16.1
[0.16.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.16.0
[0.15.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.15.0
[0.14.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.14.0
[0.13.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.13.0
[0.10.0]: https://github.com/Aanerud/arcen_public/releases/tag/v0.10.0
[0.9.8]: https://github.com/Aanerud/arcen_public/releases/tag/v0.9.8
