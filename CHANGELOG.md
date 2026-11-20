# Changelog

All notable changes to Arcen are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Arcen uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.20.5] — 2026-11-20

A Linux Pier joins your own GNOME Wayland session and keeps a joined
desktop's keyboard, pointer and pen to that desktop; Native tablet works on
joined Xorg desktops; a Windows Deck inside a Windows Pier's session no
longer makes that Pier end the session; a macOS Pier keeps host audio at the
right speed on any output rate; the Mac Deck's audio follows the default
output; certificate renewal is one journalled transaction on every Pier; and
Canvas no longer restarts near a screen edge. 0.20.5 speaks protocol v5 and
works with every 0.17.0 to 0.20.4 Deck and Pier.

### Fixed

- Canvas screen updates no longer restart every minute or so while text
  scrolls near the bottom or right edge of a screen whose size is not a
  multiple of 16 pixels, such as 1920 × 1080. A moving picture region
  that started against that edge was moved off it with a size only valid
  there; the Pier refused its own update and sent the whole screen again
  (2 to 4 times in 150 seconds on an X11 Pier). Such a region now stays
  against the edge or reopens on the block grid. Pictures stay exact.
- Replacing a Pier certificate can no longer leave a mismatched certificate
  and key behind. On Linux (`arcen-pier new-host-cert` and
  `arcen-new-host-cert`) and in the Windows installer, a run that is
  interrupted part-way is now finished or undone by the next run, so the Pier
  always has the whole old pair or the whole new one, as the Linux installer
  and the Mac already did. A second run while one is busy stops with "another
  host-certificate transaction is active" and changes nothing. Renewing never
  rewrites the key, so paired Decks keep trusting the host.
- The Windows installer (and `new-host-cert.ps1`) no longer lets another
  account on the machine get at the Pier's private key while it is written.
  It refuses a TLS or Arcen folder that is a junction or link, or that
  belongs to an account other than SYSTEM or Administrators (delete it and
  run again), creates its folders and the new key already readable only by
  SYSTEM and Administrators, and sets permissions on the folder itself
  rather than on wherever a link points.
- A Linux Pier joined to the desktop on a workstation's own monitors now
  types, points and draws into that desktop only. Its virtual keyboard,
  mouse and pen are offered to every display server on the machine, so a
  login screen or another account's desktop could also have read them. Each
  is now used only while the Pier proves that the joined desktop's display
  server is the only one reading it, checked every two seconds and whenever
  another session comes to the front. If the check fails, input stops at once
  and nothing is sent elsewhere. When it passes again within five seconds,
  held keys and buttons are let go and input resumes; otherwise the Deck is
  disconnected with the reason and the workstation is locked as on any
  disconnect. A pen that fails the check stops for the rest of the
  connection, and the Deck is told it now works as a mouse. Ctrl+Alt+F1 to
  F12 from the Deck no longer switch the workstation to another session.
  System services that read every input device, such as `acpid`, `thd`,
  `gpm` or `brltty`, do not count against the check and are logged. Another
  login session, a login screen, another display server, another account's
  program or an input remapper that resends to every session (`keyd`,
  `input-remapper`) does, and the Deck's reason names the program, its
  process and what the administrator should do. Proven with the Pier's own
  virtual devices on a lab workstation; not yet tested with a physical
  tablet.
- A Linux Pier on a host without a display manager no longer starts a new
  login session when it cannot read who is signed in. If `loginctl` fails,
  times out or gives an incomplete answer, the sign-in is now refused with
  `console_sessions_unreadable` instead of being treated as "nobody is
  signed in", so an account that already has a session never gets a second
  one. Sign in again once logind answers.
- A Linux Pier no longer ends a Deck's attachment to the user's own
  workstation session when a single check with logind fails or is slow.
  It ends the attachment only when the session is really gone, for example
  after the user logs out. If logind stops answering for about 10 to 25
  seconds, the Pier ends the attachment and hands the workstation back
  locked, as when the Deck disconnects. If it cannot be locked yet, its
  screens stay blank and its keyboard and mouse disabled until logind
  answers again. The user's session keeps running.
- A Linux Pier that creates its own Wayland desktops (a host without an
  Xorg server) no longer creates one for an account that already has a
  session at the workstation. If your own GNOME Wayland session is there,
  Arcen joins it (see Added); if it is an X11 session, the sign-in is
  refused with `console_session_unreachable`;
  and if the Pier cannot read who is signed in, with
  `console_sessions_unreadable`. An account with no session at the
  workstation still gets its desktop as before.
- A macOS Pier now picks up a renewed or replaced certificate without a
  restart: send it `SIGHUP` (`sudo launchctl kill HUP
  system/pier.arcen.tech.service`). A replacement that is malformed, expired,
  half-written or does not match its key is refused, and the Pier keeps
  serving the certificate it had. The Pier also reports its certificate as
  active, expiring, reloaded, refused or expired, as the Linux and Windows
  Piers do.
- A macOS Pier no longer sends host audio at the wrong speed when the Mac's
  output device runs at another rate than 48 kHz, for example Bluetooth
  headphones in call mode (16 or 24 kHz). Capture runs on the output
  device's clock, not the 48 kHz the audio tap reports, so 24 kHz sound
  reached the Deck at twice the speed with half of it missing. The Pier now
  reads that clock and converts it to 48 kHz (next entry).
- A macOS Pier keeps host audio on an output device running at any rate
  from 8 to 384 kHz (Bluetooth headphones in call mode, a 44.1 kHz DAC), and
  follows the device when it changes rate during a session, as a headset
  does when an app opens its microphone, without losing audio at the
  change. The conversion is a new shared windowed-sinc resampler in
  `arcen-media`; it adds 32 frames of the device's rate when upsampling
  (1.3 ms at 24 kHz, 4 ms at 8 kHz) and 0.7 ms when downsampling, and
  nothing at 48 kHz. Outside that range the session carries on without host
  audio and says why. `probe-audio` reports the clock it captured from
  (`clock_rate_hz`). The Windows Pier still uses its own resampler; it moves
  to the shared one in a later release.
- Renewing a Pier certificate keeps every name the old one covered. A renewal
  on a Mac, by the Windows installer or with the Linux Pier's
  `new-host-cert` used to drop any address or name it was not given again,
  so a Deck that connected by that address stopped connecting.
- Renewing over a key written by `openssl` (the format every Linux Pier and
  most operator tooling use) now works on a Mac and in the Windows
  installer. The key file is left exactly as it was, so paired Decks keep
  trusting the host.
- Certificates made on a Mac or by the Windows installer now match the ones
  Linux makes (`CN=Arcen Pier`, not a CA, signing only, valid from five
  minutes before issue), and each renewal gets its own serial number instead
  of repeating the last one.
- An interrupted certificate replacement on a Mac is put back together at
  the next run: the Pier ends up with the old certificate or the new one,
  never half of each.
- Two certificate runs on a Mac at the same time no longer interfere: the
  second one stops with "another host-certificate transaction is active" and
  changes nothing, as on Linux.
- A support bundle on a Windows Pier or Deck now refuses a source file that
  was replaced between being checked and being opened, as on Linux and
  macOS. The file opened must be the one checked (same volume and file
  number, read from the open file itself); otherwise the bundle's manifest
  records it as changed during read and leaves it out.
- Support bundles on every Pier and Deck now read a source only through
  folders that are really where its path says. A folder link someone else
  could have planted, or any Windows junction or folder reparse point, is
  refused (`source_path_linked`); a link the system or the collecting
  account made in a folder only it can write (macOS `/var`) is still
  followed. A FIFO put where a source was no longer hangs the bundle; it is
  refused at once as an unsafe file type. A source with a second name (a
  hard link) is refused (`source_hard_linked`). On Windows a source is
  identified by its 128-bit file ID, so ReFS files whose 64-bit indexes
  collide are told apart (a volume with only 64-bit indexes is noted as
  `source_identity_index_only`), and a source on a network or other volume
  that cannot prove which file was opened is refused as
  `source_identity_unverifiable` instead of being reported as changed.
- **macOS Deck: host audio now follows your Mac's sound output** (GAP-026
  closed). Picking another output, unplugging headphones or a USB device, or
  a failed audio stream used to leave the sound on the old device, or
  silent, until you reconnected. The Deck now moves to the new default
  output by itself (it checks twice a second), waits progressively longer
  between attempts when a device keeps failing, never plays on two devices
  at once, and never starts playing again after you disconnect. The Windows
  Deck follows its default device the same way, now through the same shared
  code. Stream Statistics shows when audio output is rebuilding and why.
  Proven by switching the default output; physically unplugging and
  replugging headphones or a USB device is not yet qualified.
- A Windows Deck running inside a Windows Pier's session no longer makes
  that Pier end and lock the session a second or two after the Deck's
  window comes to the front (for example when it goes fullscreen). The
  Deck registered for raw keyboard input, which makes Windows skip every
  low-level keyboard hook while the Deck's window is in front: the Pier's
  desk-privacy check then failed, so the Pier, correctly, ended the session.
  The same rule had switched off the Deck's own keyboard hook while its
  window was in front, so the Windows key, Alt+Tab and Print Screen stayed
  on the Deck's PC; they now go to the host again. A shortcut that another
  program's keyboard hook on the Deck's PC blocks (a Windows Pier's desk
  privacy, a kiosk lock, a hotkey tool) is no longer sent to the host:
  the Deck is never a way around such a hook. Proven with injected keys;
  not yet typed on a physical keyboard.

### Added

- A Linux Pier now joins your own GNOME session on Wayland at the
  workstation instead of refusing it: your own monitors and applications,
  nothing of the session changed. A locked session is unlocked once your
  password is accepted, and the session is locked again when the Deck
  leaves; it is never ended. A Deck that disconnects signs in again rather
  than resuming. While desk privacy is on, a session shown on a monitor at
  the desk is still refused (`console_privacy_unavailable`), because a
  Wayland desk cannot be blanked; a Wayland login screen with nobody signed
  in is still refused (`console_wayland_session`). Proven on a lab
  workstation without monitors or a login screen; not yet proven natively
  with the GDM login screen, a remote-first sign-in, a physical monitor
  plugged in during a session, or alongside the console (Xorg) mode.
- Native tablet now works on a Linux desktop Arcen joins (the account's own
  Xorg session at the workstation, or another remote-desktop product's).
  The Pier reads the pen from the tablet itself and brings it in through its
  own pen device, which it first proves only that desktop's display server
  reads; no USB device is published on the host. The Deck is told exactly
  what reaches the desktop (pressure, tilt, eraser, hover, barrel buttons as
  the tablet reports them; never pad buttons or touch), and when the route
  is refused it says why and the pen stays a mouse. The raw USB bridge stays
  refused on such desktops. Not yet tested with a physical tablet.

### Changed

- The certificate helpers no longer have their own copies.
  `arcen-new-host-cert` now runs `arcen-pier new-host-cert` and needs neither
  OpenSSL nor `flock`. On Windows, `new-host-cert.ps1` now runs the new
  `install-arcen-pier.exe --tls-only`, which also takes `--renew`. Every
  certificate they make is the one the installers make (`CN=Arcen Pier`,
  valid from five minutes before issue), and a new Linux key is PKCS #8.
  Without `--directory`, `arcen-new-host-cert` now uses the Pier's
  `/etc/arcen`, or `ARCEN_TLS_DIRECTORY`, instead of `/etc/arcen/tls`. On
  Windows, the shared `host.cert-sha256`, `host.spki-sha256` and
  `host.generated-by-arcen` replace the earlier helper's `.txt` files, which
  are removed. `install-arcen-pier.exe --tls-only` restricts only the `tls`
  directory it is given, not the folder above it, unless that is
  `%ProgramData%\Arcen`. The helper no longer looks for the installer in a
  folder relative to itself; name a build elsewhere with `-Installer`. If
  an earlier `new-host-cert.ps1` run was interrupted, the installer now
  stops with a message instead of making a new key.
- A Linux Pier now identifies the account's own GNOME Wayland session at the
  workstation and checks, read-only, that its compositor belongs to that
  session and account; the Pier's log names the session and the result of
  the check (GAP-042).

## [0.20.4] — 2026-11-20

One keyboard switch on both Decks, with a chord you can choose; slow, fine
scrolling now arrives from both Decks; a Windows Pier locks the attached
session even when its service dies; a wide-colour Windows display starts an
ordinary session at once; and a Linux Pier claims pen support only with a
working virtual tablet. 0.20.4 speaks protocol v5 and works with every
0.17.0 to 0.20.3 Deck and Pier.

### Fixed

- A Linux Pier without a working virtual tablet no longer advertises pressure,
  tilt, eraser or proximity support. Mouse and keyboard remain available.
- A Windows Pier whose display shows wide colour (WCG, such as Auto Color
  Management) without HDR now starts an ordinary session straight away. It
  used to wait about 25 seconds for a plain SDR mode that the display never
  enters. HDR sessions still start only when Windows really shows HDR.
- Slow, fine scrolling now arrives on both Decks. On a Mac, a mouse wheel
  with smooth scrolling that moved less than a whole line did nothing, and
  very gentle trackpad movement could be lost before it added up to a
  scroll step. On Windows, a precision touchpad's smallest movements were
  rounded away for a remote computer that only takes whole wheel steps.
  Small movements now add up exactly, separately for each Deck window, and
  are forgotten when the Deck loses focus or reconnects.

- **Windows: a Pier service that dies while a Deck is attached no longer
  leaves the session unlocked** (GAP-047 closed). The broker now starts a
  session guard for each attachment: a LocalSystem process outside the
  broker and the agent's job, reading a pipe only the broker writes. When
  the pipe closes without a hand-back (the service was killed, crashed or
  stopped without one), the guard waits, as the live hand-back does, for
  the display watchdog to give the desk its monitors back, then locks the
  session (disconnecting it from the console if it will not lock). No
  restarted service is needed. The broker also records the attached WTS
  session, its logon, the boot and the account's SID in
  `recovery\deck-attachment.record`; a broker that starts and finds a
  record whose exact session is still alive locks it before it gives the
  desk its speakers or displays back. A record whose session id now
  belongs to another logon, account or boot locks nobody, and an unreadable
  one is kept aside without locking anyone. Applications in the session
  are never ended. The desk's speakers stay silent until the service runs
  again.
- **Windows Deck: the root window's monitor in the per-monitor statistics.**
  With Match my layout, the session's root window counted its frames only
  for Stream Statistics; the shared per-monitor log line
  ("per-monitor stream delay") read 0 presented fps for that monitor however
  well it ran. Every window now reports its new frames through one shared
  writer, once per frame, and a re-present of a frame already shown no
  longer counts as a new one (submissions, not a measurement of the glass).
  A new Match my layout commit restarts the counts instead of reading
  "nothing presented" against the old total, on both Decks.
- **False "unpresented" health on a steady macOS Deck stream.** The macOS
  Deck counted its presenters' outcomes for session health only when it
  wrote its once-a-second log line, so each five-second window held four or
  five seconds of presents against five of decoded frames: a stream whose
  every frame was confirmed on glass alternated between "presented 175 of
  150" and "140 of 150, Critical". It now takes them every UI pass, as the
  Windows Deck does; the log line keeps its cadence.

### Changed

- **One keyboard switch, on both Decks.** Ctrl+Alt+Home (⌃⌥Home on a Mac)
  flips the keyboard between **Remote** and **Local** (this computer); a
  click into the picture also returns to Remote and is not sent. In Remote,
  every key and the system shortcuts go to the remote computer: Alt+Tab,
  Alt+Shift+Tab, the Windows key and Print Screen, and on a Mac ⌘Tab, ⌘Space,
  Mission Control and now ⌃← and ⌃→ (with the ⌘Tab switch on). In Local they
  all stay on this computer. Every flip shows a short notice ("Keyboard:
  Remote — Alt+Tab goes to Z8" / "Keyboard: This computer") besides the
  indicator.

### Added

- **Settings › Input › Keyboard switch** (both Decks): choose the switch's
  chord: Ctrl+Alt+Home (the default), Ctrl+Alt+Shift+Z, Alt+F1 (GNOME uses it
  for its Activities overview) or a typed chord. Shared code refuses chords
  the system never passes on (Win+L, Ctrl+Alt+Del, ⌥⌘Esc, ⌃⌘Q), plain typing,
  the Deck's other chords and Mac system chords, and warns when a chord is
  one a professional application uses. Stored in settings.json as
  `keyboard_switch` (`"Ctrl+Alt+Home"`); the Help sheet, menus and indicator
  name the chosen chord.
- **Who has the keyboard (Mac Deck).** The Mac Deck uses the same shared
  model: **⌃⌥Home** (Fn+⌃⌥← on a laptop) gives the keyboard to this Mac,
  releasing every key held on the host first, and back; a click into the
  picture gives it back and is not sent; **Connection › Keyboard: Host or
  This Computer** does the same; the menu bar says who has it ("Keyboard: Z8
  (Control-Option-Home to release)" / "Keyboard: this Mac (click to
  return)"). While this Mac has the keyboard nothing typed reaches the host
  and ⌘H hides the Deck again.
- **⌘Tab, ⌘Space and Mission Control to the host (Mac Deck, opt-in).**
  **Settings › Input › Send ⌘Tab, ⌘Space and Mission Control to the remote
  computer** (off by default) takes ⌘Tab, ⇧⌘Tab, ⌘\`, ⌘Space, ⌃↑, ⌃↓, the
  screenshot keys and ⌥⌘D for the host while it has the keyboard and a
  session window is in front, through an event tap that needs the
  Accessibility permission (the switch's **Allow in Accessibility…** button
  asks, and the Deck offers it once in context, below). With it on, ⌃← and
  ⌃→ follow the keyboard switch too; ⌥⌘Esc, ⌃⌘Q and every key
  under Secure Event Input stay on the Mac. ⌘Tab is the host's app switcher:
  Alt+Tab on a Windows or Linux host, ⌘Tab on a Mac host. The decision is
  shared (`arcen_input::keyboard::MacTapRouter`); settings.json key
  `capture_mac_system_shortcuts`.
- **⌘Tab asked in context (Mac Deck).** The first time ⌘Tab takes you out
  of a session in Remote without the event tap, the Deck asks once: "Send
  ⌘Tab to the remote? macOS needs the Accessibility permission." **Allow**
  turns on the ⌘Tab switch and asks macOS; **Not now** is remembered
  (`mac_system_shortcut_offer_answered`) and the Deck stays quiet. The switch
  stays in Settings › Input. Without the permission nothing changes.

## [0.20.3] — 2026-11-18

Speed no longer tears on either Deck, the Windows Deck keeps 60 frames a
second on two monitors by decoding on a graphics device of its own, a Linux
Pier no longer captures torn fullscreen video through MIT-SHM, and every
streaming preset explains itself in one line. 0.20.3 speaks protocol v5 and
works with every 0.17.0 to 0.20.2 Deck and Pier.

### Changed

- **Speed no longer tears, on either Deck.** Speed keeps what makes it
  quick: at least 60 fps, a one-frame encoder buffer, and motion kept smooth
  on a weak link. Each frame now waits for the display's next refresh or
  composition instead of replacing a picture mid-scan, which costs about
  3 ms on average. No preset tears now.
- The unused tearing presentation timing and both Decks' tearing present
  paths were removed (internal clean-up, no change in behaviour).
- **Each streaming preset now has a one-line explanation in Settings, on both
  Decks.** The line says what the preset favours and what it gives up first
  on a weak link. For example, Auto lowers the frame rate and keeps the picture
  sharp, while Speed keeps the frame rate and lets the picture soften.

### Fixed

- **The Windows Deck no longer slows down with two monitors.** Decoding and
  showing the picture used one graphics device, so on some graphics
  drivers (the lab's virtual GPU among them) one window waiting for the
  display held up decoding and the other window: with Match my layout on two
  2560x1600 monitors, each showed about 26 frames a second and the video
  kept skipping ahead. The Deck now decodes on a graphics device of its own
  and each window shows its picture on its own, so both monitors show 60
  frames a second on the same machine, with no skipping. Graphics drivers
  that cannot pass pictures between devices exactly (older AMD and NVIDIA
  drivers, among others) keep working as before, and Stream Statistics
  says which way the Deck decodes. With Match my layout the first display's
  frame rate in Stream Statistics is no longer counted twice.
- **Fullscreen video could arrive torn from a Linux Pier's MIT-SHM capture.**
  A fullscreen OpenGL player (a broadcast monitor, a video player) that draws
  straight to the screen could be captured with the top of one of its frames
  and the bottom of the next whenever the Pier read the screen directly:
  Grading on a depth-30 desktop and the software encoder on graphics cards
  without NVENC. On the lab, up to 98 % of such pictures were torn. The Pier
  now copies the screen in one step on the graphics card before reading it,
  and no torn picture remains. The eight-bit NvFBC path (Auto and Speed on
  NVIDIA) never tore and is unchanged. No preset tears on the Deck either
  (see Speed above).

### Re-issued 2026-11-19

Every 0.20.3 download (both Decks, the USB component and every Pier) was
re-issued on 19 November 2026 with these changes.

#### Upgrade note

- **Deck chords moved to the menu.** F9 (release stuck keys) and the
  keyboard-mode and paste chords Ctrl+Win+K, Ctrl+Win+U and
  Ctrl+Alt+Win+V (on a Mac ⌃⌘K, ⌃⌘U and ⌃⌥⌘V) are no longer kept by the
  Deck: they reach the host, where Maya, Resolve and Flame use them. Their
  actions are **Connection › Release Stuck Keys**, **Keyboard: Physical
  Keys / Characters** and **Paste as Keystrokes**. Help › Keyboard
  Shortcuts lists what the Deck still keeps. The Windows Deck's **Keyboard
  Grab** item is now **Keyboard: Host or This Computer** (Ctrl+Alt+Home).

#### Fixed

- **Pen barrel buttons (Tablet support).** A Windows Deck's Wacom barrel
  button (Windows Ink: Windows' right click) middle-clicked on a Linux Pier,
  and a pen button the Wacom driver performs as a mouse click (Middle Click,
  Right Click, Left Click) was dropped while the pen hovered. `buttons` in
  `pen_event` now names what a button does (bit 0 right click, bit 1 middle
  click) on every Pier, which says so with the new `pen_button_actions`
  capability: Linux presses `BTN_STYLUS2` (button 3) and `BTN_STYLUS`
  (button 2), Windows the Windows Ink barrel and a mouse middle click, macOS
  right and other mouse events. Both Decks honour the tablet driver's own
  clicks while the pen is in range and add Settings › Input › Tablet support
  pen-button choices (Right click, Middle click, Off: the Windows Ink barrel;
  a Mac pen's lower and upper side switch). A Deck swaps the two bits for a
  Linux Pier that predates the capability, so updating the Deck alone fixes
  it there. A Windows Pier before this change pressed the barrel for the
  middle click too.

#### Added

- **Who has the keyboard (Windows Deck; shared model).** The host has the
  keyboard and its system shortcuts (Alt+Tab, Alt+Shift+Tab, the Windows
  key, Print Screen) while a session window has focus, windowed and
  fullscreen, as before. **Ctrl+Alt+Home** gives the keyboard to this PC,
  releasing every key held on the host first, and back; a click into the
  picture gives it back too and is not sent to the host. The menu bar says
  who has it ("Keyboard: Z8 (Ctrl+Alt+Home to release)" / "Keyboard: this
  PC (click to return)"); in fullscreen the connection bar does while this
  PC has it. **Settings › Input › System shortcuts go to** (the remote
  computer by default, when fullscreen, or this computer) replaces the
  unsaved keyboard grab and reads its `always`/`fullscreen`/`never` tokens
  (`keyboard_capture` in settings.json). The decisions are shared
  (`arcen_input::keyboard::KeyboardCapture`).
- **Connection › Send Key** on both Decks: Print Screen, Scroll Lock, Pause,
  Insert, Ctrl+Alt+Del, Menu and F13–F24, each a press and release after
  every held key is let go (shared `SendKey`). Key remaps are validated in
  shared code and can never send a power, sleep or wake key or SysRq
  (`KeyRemapTable`).
- **Help › Keyboard Shortcuts** on both Decks, generated from the shared
  tables, and a Settings line that says where the system shortcuts go.

- Windows Pier: hardware encoding on GPUs without NVENC through the vendor's
  Media Foundation hardware encoder MFT (Intel Quick Sync, AMD AMF, any vendor
  that registers one), labelled **Alpha**. Auto now tries NVENC, then this
  encoder once a per-session admission probe has proven an encode by its SPS
  (H.264 or HEVC, 8-bit 4:2:0), then OpenH264. Capture stays on the GPU
  (DDA/WGC, D3D11 video processor to NV12). READY, the hello, the
  served-pipeline update, Stream Statistics and `diagnose-host` name it
  "Media Foundation hardware (Alpha)". `platform.media_foundation_hardware:
  false` turns it off; `video.encoder: "mf-hw"` pins it. Grading and HDR stay
  refused on it (`gpu_format_unsupported`); multi-monitor sessions use
  OpenH264.
- **Every Pier: F13–F24 and the Menu key reach the host, and the hello says
  which PC keys a Pier takes.** The Linux Pier presses F13–F24 and Menu
  (`KEY_COMPOSE`) on Xorg and Wayland, and on Xorg makes F13–F24 type the
  keysyms F13–F24 while a Deck uses them (stock keymaps say `XF86Tools`,
  `XF86Launch5`…). The Windows Pier injects `VK_F13`…`VK_F24` and
  `VK_APPS`. The macOS Pier takes F13–F20 and the Menu key as macOS's own
  PC-keyboard code; a Mac has no F21–F24, Insert, Print Screen, Scroll Lock
  or Pause. One shared table holds every Pier's code, and the new hello
  field `pc_keys` lets a Deck grey out Send Key items a host would drop.

#### Removed

- The comparison-only Media Foundation software H.264 path in capenc
  (`--features mf`).

#### Added (not yet enabled)

- Canvas v1 wire vocabulary for damage-first software streaming (ADR 0028):
  the `canvas_v1` capability, the per-monitor canvas status in the Pier's
  hello, the `0x0E` canvas record (reset barrier and fenced frames of fill,
  copy, tile, cache and surface commands) and the Deck's `canvas_ack` and
  `canvas_reset_request`, all bounded before allocation. No product sends
  them yet. The unused partial-frame type `0x02` is removed and reserved.
- The shared canvas interpreter (`arcen_media::canvas`): one state owner for
  the Pier's mirror and every Deck, a CPU executor, the canvas digest (the
  lossless-refinement digest over exact blocks), an eight-bit BGRA
  refinement source and depth-8 `delta-zstd1` tile packing. A corpus replay
  proves the Pier's mirror equals an independent replay of the wire bytes
  for every frame. No product uses it yet.
- The canvas planner and CPU-budget governor (`arcen_media::canvas`): one
  monitor's Keel facts (damage, history, traits, verified moves, classes,
  motion regions) become copies, fills, cache paints, exact tiles, motion
  surfaces and refinement, through a surface-encoder seam; one budget per
  Pier (`video.software.cpu_share`, 50 % of the logical cores by default)
  is shared by every monitor and spent mandatory work first, then exact
  updates, then surfaces, then refinement. Surfaces degrade rate first,
  then quality, then size, before text waits, and at most two frames with
  optional work wait for a Deck's acknowledgement. A simulation on Keel's
  work meter and corpus replays over Keel's scenarios gate it. No product
  uses it yet.
- The Deck's shared canvas (`arcen_deck_core::canvas`): canvas records are
  admitted per monitor (the hello's state, then region-frame admission),
  validated by the shared state owner, decoded (tiles, and one H.264
  decoder per surface, at most two per monitor) and handed to the
  presenter's new canvas executor before it presents; `canvas_ack` at least
  four times a second, `canvas_reset_request` after any loss, a 100 MiB
  cache (16 MiB floor), the canvas digest, a Settings switch (on by
  default) and a "Screen updates" statistics line.
- The Windows Deck's canvas executor (ADR 0028): one eight-bit D3D11
  canvas per monitor on the presenter's device (fills, copies, lossless
  tiles, the cache, and H.264 surface paints converted by the presenter's
  own NV12 pass), presented by copying it into the eight-bit swapchain and
  read back for the canvas digest. Tested against the shared CPU executor:
  lossless blocks bit-exact, surface blocks within one code, on WARP and
  GPUs. The Windows Deck asks for canvas in single-window sessions.
- The macOS Deck's canvas executor: one persistent eight-bit Metal canvas
  per monitor, written by fills, copies (overlaps staged through a
  temporary), tiles, cache stores and paints, and surface paints that
  convert the decoded H.264 picture with the shared colour conversion on
  the listed blocks only; the Auto/Speed layer presents it. Tested against
  the shared CPU executor on the planner's corpus: exact on every lossless
  block, within one code on surface blocks. The macOS Deck asks for canvas
  on single-window connections (Settings switch, on by default); a
  multi-monitor connection is served full frames.
- The Pier side of the canvas (ADR 0028): every Pier's software capture
  (Linux Xorg and Wayland, Windows) now runs one shared software loop. For a
  Deck that offers canvas on a single display it sends canvas records under
  the CPU budget (`video.software.cpu_share`) and says so in its hello; any
  other Deck gets the same full-frame stream as before, from the same loop.
  Hardware encoders are unchanged. Canvas video regions follow the bitrate
  and frame rate the Pier grants them, and a dragged window no longer
  reopens its encoder on every small change.

#### Fixed

- **Both Decks: a saved connection says which Displays choice it connects
  with, next to Connect.** A connection saved with its own Displays choice
  (Match my layout) still opened every monitor with Settings › Displays on
  Primary display only, and only its editor and Settings said why. Home's
  panel for the selected connection now has a Displays line: the mode, and
  "this connection's own choice; overrides Settings: …" when it wins over
  the setting, with a **Use Settings** button that hands it back.
- **Windows Deck: release builds route no injected key as a system
  shortcut.** The input lab's mark (`LAB_KEY_MARK`) exists only in
  `--features dev-tools` builds.
- **Windows Deck: the keyboard hook runs on a thread of its own.** The hook
  that takes Alt+Tab, Alt+Shift+Tab, the Windows key and Print Screen for
  the session (Keyboard grab, on by default) ran on the window's thread.
  Windows skips a low-level hook whose thread does not answer within its
  hook timeout (and, since Windows 7, removes it silently), and that thread
  is busy drawing; a skipped hook leaves those keys on the Deck's PC (Alt+Tab
  switches its windows, Print Screen opens its screen capture). The hook now
  runs on a thread that only pumps messages, still installed only while a
  session window has focus. The Deck log says when routing starts and
  stops, with how many keys it took and passed (no key is named). Proof on
  a Windows Deck with a real keyboard is still pending.
- **Linux Pier: committed text never presses the SysRq key.** On X11 the
  Pier types committed text on whatever keys the session's keymap gives
  the characters, and any X client may change that keymap. A keymap that put
  a character or a modifier on keycode 107 made the Pier press `KEY_SYSRQ`
  on its virtual keyboard, which the kernel's SysRq filter reads; with an
  Alt key remapped as Shift that could start a SysRq action. That key is
  now never used, as Print Screen already never was.
- **Linux Pier: a Deck's keys never reach a text console.** The session's
  virtual keyboard is a kernel keyboard, so with a text console in front
  (someone at the desk switched to one) a Deck's Ctrl+Alt+Del would have
  been the console's reboot, Alt+F*n* a console switch and typing input to
  its login prompt. The Pier now sends key input only while a display server
  owns the screen, and lets go of held keys when that stops. Every X server
  the Pier starts also refuses the Ctrl+Alt+Backspace zap (`DontZap`), and
  committed text no longer borrows F13–F24, which a Deck now presses.
- **macOS Deck: F13, F14 and F15 are Print Screen, Scroll Lock and Pause on
  a Windows or Linux host.** A Mac keyboard has no Print Screen or Scroll
  Lock. An Apple extended keyboard has F13–F15 in their places, and macOS
  reports a PC keyboard's Print Screen, Scroll Lock and Pause as F13–F15. A
  Mac host still gets F13–F15.
- **Grading or HDR with Match my layout on a Wayland Linux Pier no longer
  fails to start.** A Wayland Pier serves several displays Grading and HDR
  only once it proves them, so it serves such a session as Auto. The Deck
  asks for full colour (4:4:4) on every display for those presets, and the
  Pier kept that requirement for the Auto stream: the connection closed
  during setup with "encoder capacity admission rejected", or, on a Pier
  that proves no Grading at all, "display(s) require full-color 4:4:4".
  The Pier now drops the full-colour requirement together with the preset,
  and the session starts as Auto on every display, as Stream Statistics
  already says.
#### Changed

- **Desk privacy is now on by default on every Pier.** While a Deck is
  attached to a desktop that the host's own monitors show, those monitors
  are dark and the keyboard and mouse at the desk are blocked. Nobody can
  look over your shoulder or type into your session. This applies to the
  Linux Pier (a joined console, and another product's desktop, whose layout
  is still never changed), the Windows Pier (the console session) and the
  macOS Pier (the signed-in user's session). A desk that cannot be made
  private refuses the session with `console_privacy_unavailable` and the
  cause; it is never streamed unprotected. The Deck shows "Host screens are
  blanked for privacy" (or that privacy is off on the host). Leaving still
  locks the session first, then gives the desk its monitors and input back,
  restored exactly.
  **Upgrade note:** this changes the default. To keep the desk live, set
  `"privacy": {"desk": "off"}` in `pier.json`; every Pier logs the choice
  at start. On macOS, `platform.desktop.deskside.enabled: false`, which the
  0.20 package wrote, no longer turns privacy off. A macOS Pier with
  `input_backend: virtual_hid` refuses signed-in sessions until the input
  backend is `auto` or `coregraphics`, or privacy is off. A Windows Pier
  cannot darken a monitor, so it serves the session on a virtual display
  (or a GPU head with nothing connected) and detaches the desk's monitors
  from the desktop while the Deck is attached, then puts the desk's own
  monitor layout back exactly. A Windows desk whose session could only be
  shown on a monitor at the desk is refused until privacy is turned off.
  For a few seconds after a sign-in, before the monitors are detached, the
  desk can still see the session. Pen tablets and touch screens at a
  Windows desk are not blocked yet.
- **Software encoding has no fixed size or frame rate any more, and uses
  more of the CPU.** A Pier without a hardware encoder (Linux and Windows)
  used to stop at 1920x1200 and 30 fps. It now follows the Deck display up to
  what OpenH264 itself can encode (3840x2160, 60 fps), and the Pier lowers the
  frame rate to what its CPU keeps up with. Each picture is cut into up to
  four slices that are encoded at the same time, and the colour conversion is
  spread over the same threads, so a multi-core host encodes faster. The
  software encoder no longer drops pictures to save bits (it froze full-motion
  video at a few frames a second and paused after every keyframe); it lowers
  the quality instead, as the hardware encoders do. Unchanged pictures still
  cost no encode.
- The Windows Pier's software encoder now runs the same shared session as the
  Linux Pier, and reports the frames it captured to the frame-rate governor,
  which it did not before (internal clean-up; Windows software sessions are
  now governed like Linux ones).

## [0.20.2] — 2026-11-17

A field release for 0.20.1. A Linux Pier now works beside another
remote-desktop product on the same workstation (it joins that product's
desktop as a guest and never changes it), finds the monitors of the desktop
it joins at every connection, and says why whenever a sign-in fails; the
installer helps on graphics cards without monitors; a sign-in from a Deck
gets the desktop its users expect, without password prompts. Match my
layout also works with a Deck monitor turned on its side. 0.20.2 speaks
protocol v5 and works with every 0.17.0 to 0.20.1 Deck and Pier.

### Fixed

- **Match my layout failed when a Deck monitor is rotated (reported on
  0.20.1).** With a monitor turned on its side, connecting with Match my
  layout to a host that cannot rotate a display failed with "requested
  monitor … uses unsupported rotation". A Linux Pier serving Wayland
  desktops is such a host. The Deck now asks it for that monitor upright, at
  the size you see on your desk, which is what the Deck shows anyway, and
  asks for the rotation only where the host offers it (macOS, Windows and
  Linux X11 Piers). Earlier Decks had the same limit. Update the macOS and
  Windows Decks; Piers need no change.
- **A Linux Pier could not join a desktop that another remote-desktop
  product runs (reported on 0.20.1).** On a workstation where another
  product owns the desktop, connecting failed with "host closed the
  connection during setup", and the Pier's log said nothing after the
  sign-in. The other product's X server uses display `:100`, and the Pier
  only made keyboard and mouse devices for displays `:1` to `:99`. The Pier
  now joins such a desktop as a guest: it streams each screen as that desktop
  shows it and never changes its layout. Match my layout works when your
  Deck's monitors are laid out like the desktop's; otherwise the Deck offers
  Primary Display Only. While the other product's own client is connected,
  the Pier says so (`console_in_use_other_product`) instead of joining.
- **No multi-monitor offer while another X server holds the GPU (reported
  on 0.20.1).** The Pier looked for monitors once, at start, with a test X
  server that cannot run beside another one. It now reads the monitors from
  the desktop's own X server at each sign-in, so Match my layout also works
  after the login screen restarts, without restarting the Pier.
- **"Nobody is signed in" on a host whose display manager is another
  product (reported on 0.20.1).** With that product stopped, nothing shows a
  login screen. The Pier now starts your own desktop session, as on a host
  without a display manager. If the other product starts again while you are
  connected, the Deck is told (`console_other_product_started`).
- **No usable monitors on a GPU without monitors.** On a rack
  workstation whose NVIDIA GPU drives no monitor, the login screen's X
  server had no usable outputs. The Linux installer now notices it and
  recommends rerunning with `--console-outputs` and the GPU's real head
  names, which the Pier lists once it has started
  ([administration guide](docs/operations/pier-administration.md#joining-the-users-session-console-mode)).
- **A failed sign-in always says why.** Every way a sign-in can end before
  its picture arrives now gives the Deck a reason and writes one line to the
  Pier's log at every log level.
- **Password prompts for colour management and Power Off on Linux (reported
  on 0.20.1).** On a Linux host with Arcen installed, GNOME asked for an
  administrator password to "create a color managed device" (several times
  as a session signed in from a Deck started), and the desk user's own
  Power Off asked for one too. GNOME 40 and later run its shell and
  settings daemons outside the logind session, and Arcen's polkit rule
  counted every such program as remote. The rule now counts them as
  remote only while their user has an Arcen desktop or another remote
  desktop open, so a session at the desk or joined from a Deck keeps the
  user's own rights; and the user of an
  Arcen desktop may create and delete colour devices and profiles without
  a password, as a user at the desk may. Every other administrator action in a
  remote desktop still asks for an administrator. The installer and the
  Pier's start replace the rule (`polkit: updated …`).
- **A blank desktop after a sign-in from a Deck (reported on 0.20.1).** A
  sign-in while nobody was at the desk started the desktop the account's
  settings name, which for an account that never chose one is the
  distribution's default (standard GNOME on Enterprise Linux), not the GNOME
  Classic desktop the workstation's users knew: no icons, no Applications
  menu. The new `platform.session.default_desktop` (for example
  `gnome-classic`) names the desktop such accounts get. The Linux Pier
  records it in the account's settings before the login screen takes the
  sign-in, only when the account has no choice of its own, and never
  changes an account's own choice; Arcen's own login session on a host
  without a display manager honours it too, and now starts the account's
  own desktop choice where it used to start `platform.session.desktop`. The Pier log names the desktop
  every joined session runs.

### Re-issue — Fixed

The 0.20.2 Decks and the Linux Pier were re-issued with these fixes; the
Windows Pier is unchanged.

- **Linux Pier: a firmware hotkey device no longer makes Arcen think
  another remote product is in use (reported on 0.20.2).** On an HP
  workstation whose desktop another remote-desktop product owns, every
  sign-in was refused `console_in_use_other_product` even with that
  product's client disconnected: the kernel's `HP WMI hotkeys` device has no
  parent device, and any such keyboard-like device that was not Arcen's was
  read as the other product's connected client. Arcen now counts only a
  keyboard or pointer made in user space while a process that is not
  Arcen's holds `/dev/uinput` or `/dev/uhid`; firmware devices (on the
  host-platform bus, or with only a few keys or switches) and the desk's
  Bluetooth keyboards and mice never count. One remote product at a time
  still holds.
- **Linux Pier: after the Deck left a joined desktop, the desk had no
  sound and Arcen stayed in the session (reported on 0.20.2).** A desktop
  Arcen joined (the console, or another remote-desktop product's desktop)
  kept Arcen's session helpers running after the Deck disconnected, with no
  end, and their audio lease kept the user's default output on Arcen's
  silent sink and the speakers muted; the other product's client connecting
  meanwhile did not make Arcen step aside. Now the desk gets its sound back
  as it is handed back (the default output and mutes it had, or what the
  user chose since; Arcen's sink is removed), a Deck that closes leaves the
  desktop at once, and only a lost connection is held for its resume: for
  at most the reconnect window (`auth.reconnect_window_secs`), and no longer
  as soon as the other product's client connects, it starts a desktop of
  its own, or someone unlocks or signs in at the desk.
- **Linux Pier: Match my layout on another remote-desktop product's
  desktop fell back to one picture unless your displays matched it to the
  pixel (reported on 0.20.2).** That product lays its desktop out for its
  own client when it connects, and leaves it so; after switching to Arcen
  from the same computer, a Deck whose displays differed by size, scale,
  refresh or a few pixels of placement got Primary Display Only. Arcen
  still never changes that desktop, but now shows each of its monitors in
  the Deck window on the display in the same place (primary on primary,
  then left, right, above, below), each at the host monitor's own size and
  fitted into the window: 1:1 when the sizes match, scaled otherwise,
  letterboxed only when the shapes differ. A host with more monitors than
  your Deck shows the nearest ones; a host with fewer leaves your extra
  displays without a window. The Pier log, and the Deck command line,
  name the mapping and every difference.

- **A drag stopped at the edge of a display in Match my layout (reported
  on 0.20.2).** Holding a mouse button and dragging, for example a window
  from one of your displays to another, stopped the pointer at the edge of
  the first display, on every host. The macOS Deck followed a drag across
  displays only from the window on its fastest display, and only where the
  host had laid its monitors out like your desk; the Windows Deck never
  did. Both Decks now share one rule: the window you pressed in keeps the
  drag, the pointer goes to the host monitor shown on the display it is
  over (by your Deck's own arrangement, so a host that laid its monitors
  out differently is followed too), a gap between displays lands on the
  nearest display's edge, and the release lands where the drag ended.
  Update the macOS and Windows Decks; Piers need no change.
- **A display of a Match my layout session kept going dark (reported on
  0.20.2; macOS Deck).** A Pier sends a monitor's picture only when it
  changes, so a monitor whose desktop is still sends nothing for seconds.
  The macOS Deck read four seconds without a new picture in its main
  window as a stalled stream and dimmed that display under "Video stream
  stalled" until the host's desktop changed again; on a two-display Mac
  this was the display holding the main window. The Deck now shows a stall
  only when the host has gone silent, or that display's stream keeps
  arriving and nothing of it reaches the screen.
- **The macOS Deck's interface ran hundreds of passes a second in a
  multi-display session.** While you used the Mac, a 16 ms poll was read
  as "repaint now" and, with display sync off for several windows, nothing
  capped it (786 passes a second measured). It now runs at its poll rate.
- **A further display could keep a stale picture (macOS Deck).** A frame
  that arrived less than one refresh after the previous one was thrown
  away instead of shown at the next refresh, so a 30 Hz display lost a
  third of a 30 fps stream and, when that was the last change, kept the
  older picture. It now waits for the refresh; a newer frame replaces it.
- **Multi-display sessions reported their health as critical (both
  Decks).** The pictures shown on further displays were not counted as
  presented, so every Match my layout session read as 40 % or more of its
  frames unpresented.
- **The mouse on a further display of Match my layout barely worked, and
  buttons stuck on the host (macOS Deck; came with the previous fix).**
  Once the interface stopped spinning, the pointer of every window but the
  main one was read only on that window's own repaints, one sample each,
  and the interface's own follow-up repaints overwrote it: on a still host
  monitor 4-7 of 120 moves reached the host, a title-bar drag sent its
  press but not its release (the button stayed held on the host), and a
  drag could not start there. Each move, button and release of every
  window now leaves the moment it arrives, in order; keys and scroll typed
  there are no longer dropped between repaints either.
- **Holding a drag from the main window over another one released held
  keys about 90 times a second (macOS Deck).** Each repaint of the other
  window was read as the main window losing focus. Only the main window's
  own focus counts now.

### Re-issued 2026-11-18 evening

Every 0.20.2 download (both Decks, the USB component and every Pier) was
re-issued on the evening of 18 November 2026 with these changes.

#### Changed

- **Video bitrate no longer collapses on a path whose round trip drifts.**
  On the lab VPN the RTT climbs 2–3 ms a second for 15–20 s and falls back
  (15–30 ms above its minimum, with no load), and every Pier's rate
  controller read that as queueing: on the lab host, with that sawtooth
  emulated on the Pier's flow, an idle Auto stream was cut from 5.9 to
  1.2 Mbit/s five times in 90 s. The controller's baseline now follows a
  slow RTT slope (at most 4 ms/s, never a level, never while lossy, queued
  or while the stream's own load grows), and a stream that sends under a
  tenth of its target does not answer a queue it cannot be causing; the
  first interval that sends into it is cut at once. Real congestion is cut
  exactly as fast as before. Pier logs state the judged and the path's
  queue delay (`queue_delay_ms`, `path_queue_delay_ms`, `idle_queue`).
  Every Pier now tells the controller whether encoded video still waits (a
  queued frame or a write the path holds up), so a quiet second is judged
  idle on the Windows and macOS Piers too, and a writer stuck behind the
  path is never mistaken for an idle stream on any Pier. The Windows Pier's
  refinement now also waits while video is queued, as on the other Piers.
  The macOS Pier's relay now reports the video frames it has released to
  the session, as the Windows broker does, and no longer spins when the
  session's side audio channel is closed (it starved the relay's path
  reports).
- **The frame-rate governor now sees a slow encoder in a serial capture
  loop.** Software H.264 over XShm, and every loop that grabs between its own
  encodes, delivered no more frames than it encoded, so the governor held 30
  fps while about 8 were encoded. Each Pier now counts what the source
  produced while the loop was away (NvFBC `capture_behind`; Desktop
  Duplication's accumulated presents and Windows.Graphics.Capture's
  superseded frames as `capture_behind`), and the Xorg software loop states
  its pacing (`ticks_due`, `fresh_frames`, `fps_limiter`). The software
  contract's 30 fps is now governed too (it is OpenH264's ceiling, not a
  promise); Grading and HDR keep their fixed 30.
- **An Auto multi-monitor roster past H.264's picture ceiling uses a codec
  that fits.** NVENC encodes H.264 at most 4096 px a side; the shared plan
  now knows it, Auto rosters drop H.264 for a larger monitor and climb to
  HEVC, then AV1, when the Deck decodes them, and an exact H.264 request on
  a 5120 px monitor is refused by name before any encoder is probed.
- **A Linux console takes Match My Layout beyond 8192 pixels** (two 5K
  monitors side by side): each console monitor is its own stream, so only
  the console X server's own maximum (`xrandr`'s screen maximum, proved
  before anything changes) bounds the layout; a layout beyond it is refused
  `console_screen_too_large`. A dedicated Xorg desktop keeps 8192.
- **The Linux Pier no longer runs `nvidia-settings` as root against a
  joined console's X server.** It changes the console's MetaMode over
  NV-CONTROL itself, on the X connection whose authority file and server it
  has already proved, with every reply parsed and bounded. The single-display
  resize of a dedicated Xorg desktop now does the same, so no Pier path runs
  `nvidia-settings` any more; the root Pier's X connections also no longer
  wait on a display socket that accepts nothing.
- **Applications on a Linux desktop keep the user's GPU and driver choices**
  from their login shell (`__NV_PRIME_RENDER_OFFLOAD`, `__GLX_VENDOR_LIBRARY_NAME`,
  `MESA_*`, `DRI_PRIME`, …): the session bus, the activation environment and
  autostart programs get them; only the compositor (and its Xwayland) starts
  without them, as before.
- **A bridged tablet's pen on an Xorg desktop is no longer averaged**
  (`RawSample` 1): the X pointer and the ink are at the pen's latest report,
  not about one and a half reports behind.
- **A Windows Pier locks the session when the Deck leaves.** Arcen joins
  the user's session and never ends it; until now a Windows Pier also left
  it unlocked at the desk when the Deck disconnected. It now restores the
  displays, then locks the session (as the user, `LockWorkStation`; a
  session that will not lock is disconnected from the console, so the desk
  shows the sign-in screen), then gives the desk its sound back. A sign-in
  of the same account that takes the session over, or a session that
  changed at the desk, is not locked. While a Deck's session is held for its
  resume it stays unlocked for now (GAP-044).
- **A Windows host's own speakers are muted during a session**
  (`audio.local_playback`, default `muted`, as on Linux and macOS): every
  output device is muted for the whole session, detached time included,
  while the Deck still hears everything; only the devices Arcen muted are
  unmuted afterwards, also after a stopped service. A host that cannot prove
  its speakers silent refuses the session. Set `"local_playback":
  "audible"` to keep them playing.
- **A Windows Pier's console refusals lead with the shared codes**
  (`console_other_user`, `console_other_user_other_product`,
  `console_in_use_other_product`), and name another remote-desktop service
  that holds the workstation by its WinStation listener and protocol (for
  example `RDP-Tcp, rdp`). A returning Deck whose session changed at the
  desk is told `console_session_changed`. Another user's sign-in while a
  session waits for its Deck's resume is told so in the Linux Pier's words.
- **A macOS Pier locks the session when the Deck leaves** (ADR 0025), as the
  Linux and Windows Piers do: the system's own Lock Screen, read back before
  the session's displays and sound are given back. A console another account
  took over meanwhile is left alone; the login window is never locked. A
  Deck that comes back signs in at the streamed lock screen.
- **A macOS Pier keeps the Mac awake while it serves** (an IOKit
  `PreventUserIdleSystemSleep` assertion for the session) and wakes its
  displays when a Deck connects, so a Mac whose screen had gone dark is
  served instead of refused with "inventory is empty".
- **A macOS Pier counts failed sign-ins once for the whole Mac.** The login
  window and each signed-in account have their own desktop agent, and each
  kept its own back-off; the network service now applies the shared
  throttle to every sign-in it relays.
- **A macOS Pier's refusals after the password are typed**: another account
  at the Mac is `console_other_user` and names that account; a Match My
  Layout request it cannot build is refused with `multi_monitor_refused`
  before the Deck is told it signed in; a display capture or encoder that
  does not start closes the session with `capture_start_failed`.
- **macOS Deck: the main window's mouse leaves before the window is
  drawn.** Moves and clicks over the picture are sent as the window's
  update begins, as a further Match my layout window's already were, not
  from inside the drawing code; a click that comes with a key or a
  modifier change still leaves in order with it.

#### Fixed

- **Windows Deck: Match my layout presents without holding the decoder
  up, and logs where the time goes.** A field Deck with two 2560x1600
  monitors showed about 35 and 29 of 60 frames a second and video drifting
  behind the sound until it jumped. Each window's presenter waited for the
  display's next refresh, and presented, while holding the graphics
  device's shared lock, so the other window and the decoder waited behind
  it; it now does both holding none of the Deck's locks and shows the
  newest frame. A further window's frame also waited for the main window's
  next screen update (one a refresh, in its phase); the media worker now
  hands it over at once, as the macOS Deck does. The decoder and each
  presenter log their timing every five seconds. Still open, measured on a
  virtual GPU: with two windows on two displays the `Present` call itself
  blocks for 15-16 ms and holds the device, so the two monitors decode
  about 60 frames a second between them; the skip-ahead below keeps the
  picture with the sound meanwhile.
- **Both Decks: Auto and Speed skip ahead when this computer cannot decode
  every frame.** The video inbox keeps half a second per monitor so a burst
  after a network stall is decoded, and a decoder slower than the stream
  filled it too, the picture drifting behind the sound until it overflowed.
  When two batches in a row waited over 100 ms, and the second no less, the
  monitor's video now skips to its next keyframe; Stream Statistics says how
  often (`Decoder: fell behind the stream …`). Grading and HDR still show
  every frame.
- **Windows Deck: Alt+Tab, the Windows key, Alt+Esc and Ctrl+Esc go to the
  host whenever a session window has focus**, windowed too, not only in
  fullscreen. The Deck's own chords still reach the Deck (Ctrl+Alt+Home,
  Ctrl+Alt+F12, F9), a click elsewhere or Session › Keyboard Grab gives the
  shortcuts back, and Windows+L and Ctrl+Alt+Del stay with the PC. The hook
  takes a key only while the session window is in front.
- **Both Decks: Security mode High says why it refuses a host.** High
  accepts only a certificate from an authority the computer trusts; an Arcen
  host's own self-signed certificate failed with "invalid peer certificate:
  UnknownIssuer". The Deck now says what High means, why this host fails it,
  and what to do (a certificate from a trusted authority, or Medium).
- **Both Decks: Settings › Displays says when a saved connection keeps its
  own choice.** A connection saved with its own Displays choice connects
  with it whatever Settings says, so Primary display only still opened every
  monitor. Settings › Displays now names the selected connection's choice,
  with a button to make it follow the setting, and the log names the mode of
  every attempt and where it came from.
- **Linux Pier: Primary Display Only on another product's desktop shows one
  monitor.** A desktop another remote product laid out over two monitors
  was served whole to a Deck that asked for one display, both monitors
  squeezed into one picture. The Deck is now shown the monitor its primary
  maps to, by name and at its own size, with its pointer and pen on that
  monitor; the desktop is never changed.
- **Linux Pier: Native tablet on the desktop of the machine's own monitors
  is refused up front, saying why.** Its X server is not one Arcen starts,
  so a bridged tablet cannot be kept to the session; the bridge used to
  start and then fail with "not a dedicated Xorg desktop display". Tablet
  support works there.
- **A Windows host gives every display the Remote UI scale you chose.** With
  Match my layout and two or more displays a fixed Remote UI scale was set on
  the wrong display or refused: Windows renumbers its displays while it
  applies a layout, and the Pier looked each display up by the old name. It
  now finds each display by its stable target, so a 2×3840×2160 layout no
  longer fails with "did not settle at 100%", and a 1.5× Retina display at
  125, 175 or 250% no longer refuses the session. Windows has only fixed
  steps (100–500%) and caps each display by its size, so the Pier sets the
  nearest step that display offers (125% on a 1.5× Retina is 175%; 250% is
  300%) and both Decks name a display shown at another scale than asked. A
  single NVIDIA headless display now gets its scale too.
- **macOS: a Pier agent that dies while a Deck is attached no longer
  leaves the session unlocked.** When the agent's process ended (a crash, a
  kill, an installer stopping it), the window server gave the desk its
  displays and input back with the session unlocked, and the agent launchd
  started next knew nothing of the Deck. The agent now starts a small lock
  guard for each Deck attached to a signed-in session, outside its own
  process group so that launchd's signal to the dead agent's group does not
  reach it. When the agent's pipe closes without a hand-back, the guard
  darkens the desk again (with desk privacy on), locks the session, proves
  the lock and gives the displays back. The agent also records the
  attachment, and a restarted agent that finds its own login session's
  record locks the session before it serves anything (GAP-048). The Linux
  Pier already locked a console left by a launcher that died; that recovery
  is now covered by regression tests. A Windows broker that dies is still
  GAP-047.
- **Linux: a joined console's windows go back where they were.** After a
  Deck's multi-monitor layout on an X11 console, the console's own layout
  came back, but nothing put back the windows the layout had moved off a
  hidden or resized monitor; that was left to the window manager. The Pier
  now records the session's windows before the layout and once it has
  settled. On hand-back, or in the Pier's recovery, it moves back each window
  the layout moved and the Deck's user did not. This happens behind the lock
  screen, while the monitors are still blank (GAP-046). Measured on the lab
  host (GNOME Shell 40): the window manager already moves windows back when
  their monitor returns, so the Pier found nothing to move there. The Pier
  does the same for any EWMH window manager.
- **macOS: no restart needed after installing the Pier.** After every
  package install the login-window agent could refuse every session ("The
  host could not mute its own speakers") until the Mac was restarted: the
  installer restarted Core Audio and went on at once, and launchd started the
  agent while Core Audio was still restarting. The installer now waits for
  Core Audio to serve again with the Arcen microphone loaded, the agent no
  longer opens Core Audio before a Deck connects, and an agent whose taps
  Core Audio does not know restarts itself. A capture start that waits on
  the System Audio Recording prompt no longer refuses a session whose
  speakers are already proved muted: it is served without sound, so the
  prompt can be answered from the Deck.
- **macOS: a pen leaving the tablet** now reaches browsers as
  `pointerout`/`pointerleave` (Chrome on macOS reads it only from a mouse
  event with the tablet-proximity subtype, which the Pier now posts beside
  the tablet proximity event).
- **A rotated monitor on a Linux dedicated Xorg desktop** is planned from
  its native panel size, as on a joined console: a portrait monitor stood
  in the X screen landscape before.
- **A Linux desktop ended on the host while a Deck was attached** is
  reported as "the desktop session on the host ended", not as "video
  capture keeps failing on the host".
- **Linux Wayland session helpers retry a socket call a signal
  interrupted** (keymap reader, tablet proof) instead of failing.
- **Linux: the login screen did not come back after a session ended.**
  On GDM 40 hosts (RHEL 9 family), ending a session a remote-first sign-in
  started could leave the monitor on an empty text console: GDM opened its
  new login screen but lost the switch to its terminal (a race with logind
  restoring the ended session's terminal, opened by a login screen left
  closing), and every later remote sign-in was refused
  `console_sign_in_unavailable` until GDM was restarted. The Pier now brings
  that login screen to its terminal itself, between sign-ins and before a
  remote-first sign-in, gives the sign-in token again to a login screen that
  had only just started, and ends the processes a finished login screen
  left behind. Earlier releases had the same fault.
- **Linux: the keyring stayed locked after a remote-first sign-in.** A
  sign-in through GDM's login screen, or into Arcen's own login session on
  a host without a display manager, now unlocks the login keyring with the
  password the Pier checked (creating it when there is none), as a sign-in
  at the login screen does.
- **Linux: an unlocked session hidden behind the login screen.** When a
  joined GDM session would not lock as the Deck left, the Pier put the login
  screen in front but left the session usable one key chord away. It now
  stays blank, with the desk's keyboard and mouse disabled in it, until it
  locks (at most a minute, for a session whose lock screen is turned off).
- **Linux: Arcen's login session on a host without a display manager** is
  created at the size of the first Deck display that signs in, with that
  display's EDID, instead of the template's head size.
- **Auto follows the display that shows the picture, each monitor its own.**
  A Pier used to serve every monitor at the refresh of the Deck's primary
  display, so a 30 Hz main monitor held a 120 Hz built-in display at 30 fps.
  Now a single stream follows the display its window is on, and with Match
  my layout each monitor is served at its own display's refresh (also for
  an older Deck's Match my layout, at the refresh of the mode it asked for).
  The Deck's panel refresh is a new, optional field: an older Pier ignores
  it and serves at most 60, as before. A windowed session now asks for a
  remote display mode of at most 60 Hz too, like the other Displays modes.
- **120 and 144 Hz Deck displays are served above 60 fps.** Both Decks now
  state each panel's own refresh (a ProMotion panel's maximum, 120 Hz) apart
  from the remote display mode they ask for, which stays at most 60 Hz so
  Retina modes keep fitting an EDID timing. Every Pier serves that refresh
  where its head can run it and says "limited by the host's capture" where
  it cannot.
- **A Windows Pier creates every display of a multi-monitor session at the
  rate it is served at.** A 120 Hz Deck monitor's virtual display used to
  stay at the 60 Hz mode the Deck asks for, which capped its stream at 60;
  it now runs at 120 Hz where an EDID timing at its size carries it, as a
  single display already did (HDR keeps the refresh its 10-bit link was
  admitted at).
- **Health judges each monitor at its own rate.** A multi-monitor session
  whose displays run at 30 and 120 Hz owes 150 frames a second, not twice
  the primary's rate, so a fast monitor that falls behind is no longer
  invisible.
- **The timeline that keeps monitors in step holds a moving monitor only for
  another moving one** (ADR 0026 amended). A monitor sending only its
  keepalive, or a few frames while a window crossed its edge, used to set
  the playout delay, which pushed the monitors further apart than no
  timeline at all on a Windows Pier with uneven motion. A monitor that has
  just caught up after a burst stops holding the others at once.
- **Linux X11 heads are paced from the served rate, not the Deck panel.** A
  joined console's heads and a dedicated desktop's head run at twice the
  served rate (else the rate) where the output allows: a 30 Hz Deck monitor
  no longer leaves the host's desktop animating at 30 Hz.

#### Fixed

- **The macOS Deck ran far more screen updates than the display can show
  while the mouse moved in a Match my layout window.** Every mouse event
  started three updates of the Deck's window (165 a second at 55 events a
  second), because the window that shows several displays does not wait for
  the display. Updates of the main window now begin at most once per refresh
  of the display it is on, on both Decks; mouse and keyboard input that
  arrives in between goes out with the next update, no more than one refresh
  later, as in a single-window session.
- **macOS Deck: pointer lock in a further Match my layout window sent the
  mouse's movement after its clicks.** Movement now goes to the host in the
  order it happened, between the presses and releases it came between.
- **Windows Deck: Stream Statistics made the video later.** Its text
  changed on every update, so the panel was redrawn with every video frame
  and moved the picture's deadline by about 2 ms. Both Decks now refresh
  the panel's text four times a second.

- **A fresh sign-in of the same account pre-empts its own detached session
  on a Windows Pier, logged as a pre-emption** (issue #134): the waiting
  session is handed over at once, unlocked, instead of reading as a failed
  resume. Another account still waits for the reconnect window, and an
  attached Deck is never pre-empted.
- **A Windows Pier no longer discards display modes whose size is not a
  multiple of 16** (issue #125). The filter served the Media Foundation
  encoder, which the Pier no longer uses; OpenH264 pads and crops itself.
  A 1792x1120 request on a host without NVENC now lands on 1680x1050 rather
  than a smaller aligned mode.
- **`install-arcen-pier --uninstall --purge` keeps the Windows Deck's USB
  component state** (`%ProgramData%\Arcen\USB`) on a machine that has both.
- **An idle desktop no longer reads as critical health.** A still desktop
  sends only what changes plus a keepalive a second, and health judges frame
  rate against the content that changed — but a multi-monitor session on a
  Linux or Windows Pier proved nothing about its content, so two still
  monitors were judged against the full target (`HEALTH_CRITICAL
  dominant_cause=fps value 1 < 42` on every run against a two-monitor Linux
  workstation). Every Pier now judges a multi-monitor session on all its
  monitors' frames, each owing only what its own capture proved, against the
  target times the monitors, as the macOS Pier already did; moving content
  the pipeline falls behind on, dropped frames, latency and unpresented
  frames still raise `degraded` and `critical`. `health_stats` reports each
  stream's own rate, which is what a Deck paces each monitor by (a macOS
  Pier's multi-display sessions used to report the sum).
- **The Linux NvFBC capture counts the content it fell behind on.** Its
  encode loop grabs between encodes, so an unchanged grab did not prove the
  screen was idle; it now also counts the frames the NvFBC server generated
  while the loop was away encoding (`capture_behind`), and health judges
  against the changes seen plus those missed.
- **The Windows Pier's `health_stats` carry the network path** (`path`), as
  the documentation said: the broker fills it in, as the macOS network
  service does.
- **A joined Linux console is never served faster than its outputs
  refresh.** Auto on a console whose monitor runs at 60 Hz is served 60, with
  the capture named as the reason, rather than promising a faster Deck
  display's rate.

- **macOS Deck: Auto showed about a third of its frames on a scaled or
  windowed display.** On a 60 Hz display that macOS composes (a window, or
  a scaled mode such as a 4K panel that "looks like 2560x1440"), a picture
  takes two to three refreshes to reach the screen, and the Deck kept only
  one in flight, rendering the next only on the following refresh: 20 of
  40-48 frames a second were shown, and the session read `HEALTH_CRITICAL
  unpresented`. A frame now goes the moment a drawable is free, and Auto
  keeps a second picture in flight where it measurably takes longer than a
  refresh to reach the screen (display sync stays on: nothing tears).
  Measured on such a display at 60 fps: full screen 48 -> 95 % of frames
  shown at the same latency; windowed 46 -> 64 %, about one refresh later.
- **macOS Deck: Grading after signing in through the Deck stayed 8-bit.**
  When a macOS Pier's sign-in screen handed over to the desktop, the
  reconnected Grading session fell back to the eight-bit presentation
  (`NoRootLayer`) for good: its first ten-bit frame reached the presenter
  before the ten-bit layer was attached. It now waits for its layer.

#### Added

- **The Windows Deck names an older copy that may run instead of it**, as
  the Mac Deck does: another running `arcen-deck.exe` from another folder,
  or an installed Arcen Deck of another version or build, is named at start.

- **Both Decks say when a host kept its own layout**: a Match my layout
  session whose host shows some displays at another size, or not at all
  (another product's desktop), names them once as a notice and keeps the
  line in Stream Statistics (the Deck CLI already printed it).
- The Deck CLI smokes take `--remote-ui-scale auto|PCT`.
- **A display the host cannot keep up with is named, with the reason.** A
  Linux or Windows Pier reports each monitor's served and sent rate in
  `health_stats` (`monitors`), and a monitor that sends below what its
  content changed at for two windows running is marked `limited_by:
  host_shortfall` and logged on the Pier. Both Decks show it in Stream
  Statistics ("Host displays: 1 60/60 · 2 41/60 limited by the host keeping
  up") and log each change.
- **A Deck re-reports the display its window moved to.** Both Decks tell
  the host when a single-stream session's window settles on another display
  (or that display's refresh changes), with the new additive
  `display_refresh_update`, sent only to a host whose hello states
  `stream_rate.follows_display_moves`. No Pier states it yet: following the
  move on each Pier is tracked work (`fps-window-moves-pier-*`).
- **The macOS Pier hands the console back in the shared order** (GAP-041
  closed): lock, lock screen, displays, sound, input, as on Linux.
- **Desk privacy on a macOS Pier** (`platform.desktop.deskside.enabled`,
  off by default, as on Windows; GAP-014 closed): while a Deck is attached
  to a signed-in user's session, every display but the Pier's own virtual
  ones goes dark (an all-zero colour table, read back and re-asserted every
  100 ms; a display that stays lit for two seconds ends the session) and an event filter drops every keyboard, pointer, scroll, tablet,
  gesture and system-key event the Pier did not post. The desk gets them
  back in the shared hand-back order once the session is locked (a session
  that will not lock stays private, retried, for at most a minute), at
  once if the console switches to another account (the Deck's session then
  ends), and when the helper process ends for any reason (a crash hands the
  session back unlocked). A session that cannot be made private is
  refused `console_privacy_unavailable`. `arcen-pier-macos
  probe-desk-privacy` measures it on a Mac. Not covered: keystrokes into a
  password field (Secure Event Input bypasses every event filter), Siri,
  Voice Control and dictation at the desk, and displays fed by screen
  capture (DisplayLink, Sidecar, AirPlay).
- **The macOS Pier names a display it cannot keep up with, too**: a
  multi-display session reports each display's served and sent rate in
  `health_stats` (`monitors`) like the Linux and Windows Piers, a display's
  content rate counted only from its own `ScreenCaptureKit` stream's
  evidence that the screen was still (GAP-045 closed). A still display
  now sends a keepalive once a second, as on the other Piers, so it is not
  mistaken for one the host cannot keep up with.
- **A trackpad pinch zooms on Linux and Windows hosts.** Both Piers now
  advertise gestures and turn a pinch from either Deck into Control + wheel
  at the pointer (one notch per 20 % of scale), with nothing left held when
  the gesture ends or is lost; rotate, smart zoom and swipe are declined and
  logged. Before, a pinch did nothing there.
- **`RESUME_HOLD_ENDED` (1108)**, at every log profile: why a Linux or
  Windows Pier let go of a desktop it held for its Deck's resume without a
  resume (`resume_window_expired`, `desk_took_over`, `other_product_client`,
  `native_session_ended`, …) and for how long (`held_ms`).
- The Deck's five-second `client QoS snapshot` line says why a window was not
  ok (`dominant_cause`, presented and received rates, unpresented share).
- **The macOS Deck warns at start when another copy of it may open instead**:
  a copy elsewhere with the same bundle identifier that is running, or of
  another version (a download folder, a mounted disk image), is named in the
  start status and logged.

#### Fixed (hardening)

- **macOS USB helper and fast user switching**: the helper followed the user
  who owned the console when it started, so after a switch the first user
  kept the tablet. It now admits only the console's current owner, on every
  connection and request, and ends a session within half a second when its
  user switches away; a helper started at the login window no longer exits
  and restarts until someone signs in.
- **Linux tablet bridge on a fast reconnect**: the ending bridge's seat rule
  clean-up could delete the new bridge's rule for the same desktop; it now
  removes only its own rule.
- **The Deck CLI `media-smoke` decodes HDR (PQ) sessions as the Deck does**:
  it now teaches its decoders the session colour from the host's hello
  (and prints `session-colour`).

#### Removed

- **The Piers' dormant WSS listener** (`--wss-port`, TLS 1.2, the
  `transport:wss-v1` capability) and the `wss-compat` features of both
  Piers, `arcen-protocol` and `arcen-transport`: no shipped build enabled it
  and no Deck can speak it. `tokio-rustls`, `http`, `httparse` and `sha1`
  leave the build graph. The Python `tools/session-probe`, its only client,
  is retired; the macOS Deck's `media-smoke` proves a host streams.

## [0.20.1] — 2026-11-17

**Arcen is an extension of your machine.** A Linux Pier now joins the
desktop session you already have at the workstation, with your
applications open, instead of starting a new desktop for every connection
([ADR 0025](docs/adr/0025-arcen-joins-the-users-session.md)). Signing in
from a Deck while nobody is at the desk goes through the workstation's own
login screen, so the session you later unlock at the desk is the same one.
Disconnecting locks the session; it never ends it.

Also new: a joined Linux desktop takes your Deck's monitor layout, one
stream per monitor, and every Deck keeps the monitors of a session in step
on one shared timeline
([ADR 0026](docs/adr/0026-one-timeline-for-every-monitor.md)); and Auto now
follows your display's refresh instead of a fixed 30 fps.

0.20.1 speaks protocol v5, like 0.17.0 to 0.20.0, and every addition is
negotiated: 0.20.0 Decks and Piers keep working with 0.20.1, without the new
features.

### Upgrade notes

- **An upgraded Linux Pier with an Xorg server joins the console.**
  `platform.session.mode` is new and defaults to `console`, also on an
  upgraded host whose `pier.json` does not set it. To keep a private desktop
  per sign-in as before, set `"mode": "dedicated"` under
  `platform.session` and restart the Pier
  ([administration guide](docs/operations/pier-administration.md#joining-the-users-session-console-mode)).
- **GNOME on Wayland at the desk** cannot be joined yet: the sign-in is
  refused (`console_wayland_session`). Set `WaylandEnable=false` in
  `/etc/gdm/custom.conf` and restart GDM while nobody is signed in.
- **A host without monitors** (a virtual GPU, a rack workstation) needs
  `install-arcen-pier --console-outputs 4` to serve several displays; the X
  server reads the setting at its next start, so restart the display manager
  while nobody is signed in, or reboot, then restart the Pier.
- **Domain accounts** (Active Directory through SSSD with Group Policy access
  control): the installer adds Arcen's sign-in services to the domain's
  interactive logon map in `/etc/sssd/conf.d/90-arcen-interactive-logon.conf`.
- Hosts installed from an older template keep `"fps": 60` under `video`,
  which caps Auto at 60; remove the key to let Auto follow the Deck alone.

### Enhanced

- **A Linux Pier joins your own session.** With an Xorg server
  (`platform.session.mode` `console`, the new default) a Deck sign-in joins
  the session the account holds at the workstation, with its applications
  running; with nobody signed in, GDM's login screen signs the account in,
  so a later sign-in at the desk resumes the same session. While a Deck is
  attached the workstation's monitors are blanked and its keyboard and
  mouse disabled; on disconnect the session is locked, never ended.
  Another account at the workstation, or its session in another
  remote-desktop product, is refused with a code (`console_other_user`,
  `console_other_user_other_product`), and a refusal because another
  remote-desktop service holds the workstation names that service as the
  host reports it (its PAM service and the system service that started it),
  on both Decks and in the Pier log; no product names are built in. A
  Wayland console is refused (`console_wayland_session`) until Arcen joins
  Wayland sessions. The installer adds `pam_arcen_greeter.so`,
  `/etc/pam.d/gdm-ovirtcred` and a system-bus policy for the login screen,
  and never replaces another product's.
- **Hosts without a display manager** get a real, persistent login session
  per account (`gnome-session` on `seat0`, PAM service `arcen-login`, its
  own systemd unit). A disconnect leaves it private (GNOME has no lock
  screen without GDM: its monitors stay blank and the desk's input
  disabled), a Pier restart leaves it running, and the next sign-in of that
  account joins it; another account is refused while it exists.
- **Domain accounts sign in remotely.** On a workstation joined to Active
  Directory through SSSD with Group Policy access control, a sign-in while
  nobody is at the desk was refused by SSSD, which did not know the login
  screen's PAM service. The installer now adds Arcen's sign-in services to the
  domain's interactive logon map ("Allow log on locally", nothing broader;
  removed on uninstall). An account the policy refuses is logged as
  `console_account_denied`, and the Deck says the sign-in may be refused by
  the workstation's policy, without confirming the password.
- `dedicated` (a private desktop per sign-in, its heads made to the Deck's
  displays) remains as an explicit opt-in.
- **Several monitors on a joined Linux desktop.** A Deck that matches its
  layout (Match my layout) gets one stream and one window per monitor.
  While it is attached the workstation takes the Deck's layout: sizes,
  positions (left of or below the primary included), rotation and primary,
  on the console's own outputs. When it leaves, the session is locked and
  the workstation's exact layout comes back; a Pier or launcher that is
  killed has the layout restored by the service. A console that cannot take
  the layout is put back as it was and the Deck offers Primary Display Only;
  a subset is never served silently. A single-display Deck is served the
  whole screen as one picture, scaled to fit when H.264 cannot carry its
  width (more than 4096 pixels). New installer option `--console-outputs N`
  (or a head list; `--no-console-outputs` and `--uninstall` remove it) gives
  a headless host N virtual outputs at boot
  (`/etc/X11/xorg.conf.d/10-arcen-console-outputs.conf`); it is never
  written without the flag.
- **Monitors in step.** Every Deck now presents the streams of a
  multi-monitor session on one timeline shared with the Pier: a monitor
  whose frames arrive early waits for the slowest (at most 50 ms), so all
  monitors show the same moment of the desktop. The Deck
  logs the skew (`multi-monitor timeline`). This needs no Pier change and
  works with every Pier.
- **Rotated monitors.** Both Decks report each monitor's rotation, and the
  macOS Pier now accepts a Deck monitor turned a quarter, as an upright
  portrait display.
- **Auto follows your display.** Auto was a fixed 30 fps contract; it is now
  served at the refresh the Deck reports for its display on every Pier
  (today at most 60 Hz: Decks still ask for remote displays no faster than
  60), lowered only for what the host proves: an administrator's
  `video.fps`, the encoder's limit, the capture (a Windows desktop's applied
  refresh, the macOS Pier's 60, a Wayland output without a qualified rate at
  30). A frame-rate governor steps a session down to an even divisor of its
  rate when the host encodes below it while the capture delivers it, and
  probes back up; the link still sheds Auto's frames before its sharpness.
  Speed stays the latency preset: never below 60 fps, a one-frame encoder
  buffer, sharpness given up first, and it may tear. The hello states the
  rate and its reason (`stream_rate`), `SESSION_STREAM_START` records
  `display_refresh_hz` and `fps_basis`, and Stream Statistics shows "up to
  60 · your 60 Hz display" or what limits it. Decks keep asking for the old
  rates, so older Piers serve exactly what they did. The packaged
  `pier.json` templates no longer set `video.fps`.
- For developers: `dev-tools` builds of both Decks can simulate a
  multi-monitor layout as windows on one screen (never in release builds).

### Fixed

- A refused return to a Linux session now shows its real reason. A Deck
  returning to a held console session (a resume, or a new sign-in) waits for
  the console to be made private again and is shown the typed reason when it
  cannot be (`console_session_changed`, another account, privacy), instead
  of a capture error or a timeout.
- The desk is never left unlocked and usable when a Linux session cannot be
  secured. A refusal after the login screen or logind started the session
  hands it back locked. A console session that will neither lock nor go
  behind a login screen stays unusable at the desk even after the Pier's
  launcher is stopped or killed: its desk devices are disabled in the
  session's own X server and its monitors stay blank, and the Pier retries
  every 10 s and gives them back only once the session is secured.
- A sign-in while nobody is at a Linux workstation could end on GDM's login
  screen with the new session behind it: GDM may put a new login screen in
  front when the one that took the sign-in ends. The Pier now waits for
  that, with the desk already private, and brings the session forward again
  (at most three times while it waits for the session, and three more while
  it settles).
- A dedicated Linux desktop no longer starts beside another console session
  on any virtual terminal (another product's desktop on VT 7 made the second
  X server hang until a 30-second timeout); it is refused `seat_occupied` or
  `seat_occupied_other_product`.
- A Linux sign-in that got no desktop now leaves `SESSION_AUTH_FAIL` (stage
  `desktop`) and an error line with its reason at every log profile,
  `critical` included; the launcher's errors keep their severity, and a
  dedicated X server that fails or hangs records its log tail and keeps its
  logs in `<runtime_root>/last-failed/`.

### Known limits

- **macOS Pier** remains a preview. Its 0.20.1 changes (rotated Deck
  monitors, Auto at the Deck's refresh) pass their tests but have not yet
  been proven on a real macOS host. Restart the Mac after installing or
  upgrading the macOS Pier.
- **Linux, GNOME on Wayland:** a console session on Wayland is refused with
  a reason (Arcen joins X11 GDM sessions only), and the headless Wayland
  provider still starts a desktop per connection.
- **Refresh:** Decks ask for remote displays no faster than 60 Hz, so a 120
  or 144 Hz display is served at 60.
- **Four 4K monitors** can run below 60 fps per monitor on some GPUs (the
  host's capture and encoder limit); Stream Statistics shows the rate per
  monitor, with no degradation notice yet.
- **Console monitors are found when the Pier starts.** After restarting the
  display manager, or changing `--console-outputs`, restart the Pier.
- **Keyring:** after a sign-in from a Deck while nobody was at the desk, the
  login keyring is not unlocked; GNOME asks for its password on first use.
- **A colour-management prompt after a sign-in from a Deck:** when nobody was
  at the desk, GNOME can ask for an administrator password to "create a color
  managed device" as the new session starts. Choose Cancel; the session
  works without it.
- **A host without a display manager** has no lock screen: its session is
  resumed only through Arcen while it is kept private.

## [0.20.0] — 2026-11-16

**The Windows Deck.** Arcen Deck now runs on **Windows 11 x64** as well as
on the Mac. Both Decks are built on one shared Deck core, so they have the
same screens and wording, the same four presets, sign-in, trust on first
use, reconnect, Stream Statistics and support bundle; the Windows Deck adds
only what Windows decides: its window, a D3D11 hardware decoder, a
DirectComposition presenter, input, audio and clipboard
([ADR 0024](docs/adr/0024-windows-deck.md)). It connects to Linux (X11 and
GNOME Wayland) and macOS Piers, with keyboard, mouse, pen, clipboard both
ways, host audio and microphone.

Also new on every Deck and Pier: presentation timing per preset, a
fullscreen picture that is pixel for pixel, exact pen pressure, Native
tablet on the Windows Pier with no driver, the host's keyboard layout from
every Pier, sign-in throttling and pre-sign-in limits
on every Pier, one TLS path for every Pier, private support bundles, and a
Windows host that gets its displays and windows back when a session ends.

0.20.0 speaks protocol v5, like 0.17.0 to 0.19.0, and every addition is
negotiated: 0.19.0 Decks and Piers keep working with 0.20.0, without the new
features. 0.19.1 was not published; its changes (Exact still image on by
default) are listed under [0.19.1] below and ship in 0.20.0.

### The Windows Deck

- **Two MSIs.** `Arcen-Deck-0.20.0-windows-x64.msi` installs the Deck for
  all users in `C:\Program Files\Arcen\Deck` with a Start-menu shortcut;
  the Deck itself never runs elevated. `Arcen-USB-Component-0.20.0-windows-x64.msi`
  is **optional** and needed only for Native tablet: a small LocalSystem
  service that binds Windows' own WinUSB driver to the chosen tablet for
  the length of a session, with a driver catalog signed by a certificate
  made on that PC for that purpose (its key is destroyed after signing),
  and gives the tablet its own driver back afterwards. There is no Arcen
  kernel driver. Uninstalling the component removes every driver package
  and certificate it created, or rolls back and says what is left
  ([packaging](packaging/windows-deck/README.md)).
- **Unsigned.** Arcen has no code-signing certificate. A downloaded MSI
  opens Microsoft Defender SmartScreen's "Windows protected your PC":
  choose **More info**, then **Run anyway**, or tick **Unblock** in the
  file's Properties first. The elevation prompt says "Publisher: Unknown".
  A PC with Smart App Control in enforcement mode blocks unsigned
  installers outright; turn it off to install Arcen.
- **Video.** Auto and Speed decode H.264 and HEVC 4:2:0 eight-bit on the
  GPU. Grading and HDR (HEVC 4:4:4 ten-bit) need the range-extension
  decoder on the GPU that drives the window's monitor; the Deck probes it
  with a real keyframe and tells the host what it can decode. Without it
  Settings greys Grading and HDR out with the GPU and the missing profile
  ("NVIDIA GRID V100D-16Q decodes HEVC Main and Main 10, but not 4:4:4")
  and the Deck never asks for them. HDR turns on PQ/BT.2020
  output only when the host returns PQ. The Deck uses its own D3D11 video
  decoder: no FFmpeg, no Media Foundation decoder.
- **Input.** Keys by physical position with the PC keyboard's own shortcut
  policy (Ctrl, Windows, Alt and AltGr pass through), committed text from
  the input method, Ctrl+Alt+End for Ctrl+Alt+Del, mouse and wheel.
  **Tablet support** takes the pen from Windows Ink (pressure, tilt,
  barrel, eraser) with no host driver; **Native tablet** (with the USB
  component) attaches any USB pen tablet to a Linux, macOS or Windows host.
- **Fullscreen** draws nothing over the video. The connection bar opens
  when the pointer reaches the top edge of the screen, or with
  **Ctrl+Alt+Home**, and closes with Esc.
- **Displays, as on the Mac.** The same Settings › Displays: **Match my
  layout** gives each of up to four displays its own fullscreen window,
  arranged as on the PC, on a host that offers it (otherwise the Deck
  offers **Primary display only** and remembers it for that connection,
  or, for a desktop started without the layout, reconnecting to it or
  starting fresh); **Primary display only** goes fullscreen on the
  primary display at its own size; **Windowed** asks the host to resize
  its desktop to the window once it settles, so fullscreen on any monitor
  is pixel for pixel too. Per display, **Full resolution** streams a
  scaled monitor's own pixels and **Require 4:4:4** asks for full colour;
  Remote UI scale and the per-connection Displays choice work as on the
  Mac. One shared implementation decides all of it for both Decks.
- **Files.** Settings, saved connections and trusted fingerprints in
  `%APPDATA%\Arcen\Deck`; logs and crash dumps (minidumps only) in
  `%LOCALAPPDATA%\Arcen\Deck`. No password is stored. Uninstalling keeps
  them, as on the Mac.

### Added

- **Presentation timing per preset, on both Decks.** When a decoded frame
  is shown is now part of the pipeline contract
  (`arcen_media::video::PresentTiming`), not of the platform: **Auto**
  shows each frame at once without tearing (the newest frame wins at the
  next refresh), **Speed** at once and may tear where the display path
  allows it, **Grading** and **HDR** wait for the refresh so every frame is
  shown whole. Measured in the lab against a Linux Pier, frame hand-off to
  display ([details](docs/architecture/deck-presentation-timing.md)):
  - Windows Deck, 60 Hz composed desktop, Auto: about **12–13 ms**, from
    about 26–27 ms.
  - macOS Deck, 120 Hz panel, fullscreen: Auto about **7–10 ms** and Speed
    about **4–7 ms**, from about 13 ms; windowed about 17–22 ms.
- **Fullscreen is pixel for pixel.** In fullscreen nothing is drawn over
  the video in any preset, on either Deck, and with the default Displays
  mode the stream is the monitor's own pixel size, so one Deck pixel is
  one host pixel. The Windows Deck's connection bar appears only on
  demand (above); the macOS Deck uses the system menu bar.
- **The host's keyboard layout, from every Pier.** The Linux, Windows and
  macOS Piers all tell the Deck which layout the session uses, and again
  whenever it changes (`host_keymap`, one shared implementation). With
  the Deck's Automatic setting, a character the host's layout types by key
  goes as that key and any other (for example `æ` on a US layout) as
  text, from the first key of the session.
- **Automatic reconnect on the Windows Deck.** After a network loss the
  Windows Deck shows "Lost connection, attempting to reconnect" over the
  last picture and resumes the same desktop without a new sign-in, as the
  macOS Deck does. Restarting the Deck within the host's reconnect window
  resumes the desktop too. The reconnect flow is now shared code
  (`arcen_deck_core::reconnect_flow`), used by both Decks.
- **Support bundles that keep your details private.** Every Pier and both
  Decks collect support bundles through one shared collector. User names,
  host names and IP addresses, also inside free log text, are replaced by
  codes that are consistent within one bundle (`anon:…`) and differ
  between bundles. Crash reports are **off by default** ("Include crash
  reports"), because a crash report can hold a fragment of what the Deck
  was doing, such as a password being typed. A bundle comes with a
  public summary (bundle ID, archive SHA-256, product, OS, preset,
  counts) that contains no address, host name, path or name, for a public
  problem report. On a Deck: Settings › Logs › Create support bundle….
- **Windows Pier: the host's displays and windows come back after a
  session.** When a session ends (Disconnect, lease expiry, the Deck or the
  agent dying), the Pier removes only the displays and modes Arcen created,
  checks that the host's own topology is back, and moves the windows the
  session gathered on its display back to where they were (or to the
  primary display; off-screen windows always to the primary). A session
  that is only interrupted keeps its display for the reconnect lease, so a
  resumed session finds the same layout. If the Pier was stopped
  uncleanly, it restores at service start or at the next console change
  while no session is live.

### Changed

- **Exact pen pressure on every Pier.**
  - Linux, GNOME Wayland (Mutter 46 and later): seat pen pressure arrives
    as the Deck sent it; the Pier undoes Mutter's squared pressure curve,
    and stops doing so only for a Mutter build proven to carry the
    upstream fix (a marker that matches the library and compositor
    version).
  - macOS Pier: pressure is posted as the nearest of the window server's
    255 steps instead of the step below, and tilt Y points the way tablet
    drivers report it (it was reversed).
  - Windows Deck: Tablet support delivers Windows Ink's pressure
    unchanged, in its 1/1024 steps.
- **Failed sign-ins are throttled on every Pier.** The Linux and Windows
  Piers now use the macOS Pier's back-off through one shared adapter: a
  password the operating system refuses slows the next attempt from that
  source and for that account; at most two attempts per key are checked
  at a time; IPv6 sources count by /64; `DOMAIN\user` and `user@realm`
  count as one account; a backing-off key is never evicted to make room;
  a user name or password containing a NUL is refused before it is
  checked. A throttled Deck is told so.
- **Limits before sign-in on every Pier.** Sixteen connections may wait
  at sign-in, at most two from one address (IPv6 by /64); a third from
  the same address is refused before TLS. A connection whose address QUIC
  has not yet proven must answer a stateless Retry before it holds a
  second place or once half the places are taken, so forged addresses
  cannot fill them. Shared deadlines: 15 s for each handshake message,
  60 s for a person to type a password (Linux and Windows had 30 s), 30 s
  for the operating system's password check. The macOS Pier now
  handshakes connections side by side, so one silent peer no longer
  delays every other Deck.
- **One TLS path for every Pier.** Reading, validating, serving, reloading
  and pinning the host's TLS material is shared (`arcen_transport`); only
  the file-system checks the operating system decides stay in each Pier.
  The macOS Pier now checks its TLS files' ownership, permissions and
  validity period like the others (it read them unchecked). File names,
  pin files and fingerprints are unchanged.
- **An unservable video request is refused after the password, with its
  reason, on every Pier.** Each Pier checks the credentials first, then the
  requested video, and answers once. A request it cannot serve (for
  example Grading on a PC whose GPU cannot decode HEVC 4:4:4 ten-bit) is
  reported as "the host refused the requested video: …" in the host's own
  words, instead of "authentication failed", and a macOS Pier no longer
  closes the connection without a reason. A refused request does not
  count as a failed sign-in. A Deck started with `--connect` shows the
  error on Home.
- **A Deck never asks for what its own decoder cannot decode.** Both Decks
  follow one shared rule, from their decoder probe (D3D11 on Windows,
  VideoToolbox on the Mac): Grading and HDR are greyed out in Settings with
  the exact reason, naming the GPU and the profile it lacks, and a
  connection whose saved preset the decoder cannot decode asks for Auto
  instead. Auto and Speed offer the host only the codecs the probe proved;
  Settings says why AV1 or HEVC is missing ("AV1 is not available: <GPU>
  has no AV1 decoder.").
- **Support bundles remove the account name from home folders.** In every
  Pier and Deck log, a home folder's account name becomes the bundle's
  user pseudonym: Windows `C:\Users\<name>\…` (including
  `%LOCALAPPDATA%\Arcen\Deck\Logs`), with backslashes or forward slashes,
  macOS `/Users/<name>/…` and Linux `/home/<name>/…`.
- **Native tablet takes any pen tablet.** A Linux X11 Pier takes any USB
  pen digitizer, whichever kernel driver binds it; a macOS Pier reads any
  standard HID pen itself (models with a dedicated reader, such as the
  Intuos5 touch L, get extra features).
- **Native tablet on the Windows Pier, with no driver.** A Windows Pier now
  accepts Native tablet from any Deck: it attaches nothing to Windows, reads
  the bridged pen itself with the same readers as a macOS Pier (the Intuos
  family and any standard HID pen) and injects it as a Windows Ink pen, the
  pen Tablet support uses: pressure in Windows Ink's 1/1024 steps, tilt,
  barrel button, eraser and proximity. Chrome and Firefox on the Windows 11
  lab Pier read the 0.1/0.25/0.5/0.75/1.0 ladder as
  0.1006/0.2500/0.5000/0.7500/1.0000 from an emulated Intuos5 and
  0.0996/0.2500/0.5000/0.7500/1.0000 from the generic pen, tilt exact. Both
  Piers that read the pen themselves share one implementation
  (`arcen_input::bridged_pen`, `arcen_usb_bridge::HostPenBridge`); the
  refusal that said a kernel driver was needed is gone
  ([ADR 0023 amendment](docs/adr/0023-one-native-tablet-pen-position-path.md)).
- **Tablet support follows the Deck's own pen settings.** Handedness and
  mapping come from the tablet driver on the Mac or Windows Ink on the PC;
  the Deck's View › Tablet Orientation menu is gone.
- **A Linux Pier says why it refuses host cursor rendering** in a ten-bit
  session, and the first key typed in a GNOME Wayland session is no longer
  lost.

### Security and supply chain

- `rustls` 0.23.46, with upstream's fixes for GHSA-h2v8-394m-p7vv,
  GHSA-c8m5-c952-6cm4, GHSA-w44r-93j7-qv28 and GHSA-vp88-798v-h67f.

### Removed

- **The Deck's dormant WSS (TCP) transport.** Shipped Decks have connected
  over QUIC only since the first public release (0.9.8); the
  WebSocket-over-TCP connector, its `wss-compat` build feature and the WSS
  choice on the Add connection screen of such a build are now gone from the
  macOS Deck and the shared Deck core, so the Windows Deck never has them. A connection an older Deck saved as WSS
  opens over QUIC: port 18443 becomes 18444, and its name, settings and
  remembered username are kept.

### Known limits

- **macOS Pier, pen leaving proximity:** when the pen lifts out of range,
  browsers on a macOS host get the last `pointerup` and a hover move but
  no `pointerleave`. Linux Piers deliver it.
- **Pens with Windows Ink turned off.** On the Windows Deck, Tablet support
  reads the pen through Windows Ink. A tablet whose driver has Windows Ink
  turned off reaches Windows as a mouse, so the host gets no pressure or
  tilt; turn Windows Ink on in the tablet's settings, or use Native tablet.
- **Grading after a macOS host's sign-in screen:** when the macOS Deck
  follows a macOS Pier from its login window into the signed-in desktop
  with the Grading preset, the session can be presented in eight bits
  instead of ten; Stream Statistics shows the wide-colour warning.
- **A Linux Pier that is killed** (not stopped) may leave the session's
  private desktop running until the next sign-in or idle clean-up. This
  case has not been tested yet.
- **macOS Pier after installing or upgrading the package:** it can refuse
  every session at the login window because it "could not mute its own
  speakers", until the Mac is restarted. Restart the Mac after installing
  or upgrading the macOS Pier.
- **macOS Pier, a few failures after a successful sign-in** (several
  displays not admitted, the display or the capture failing to start)
  still close the connection without telling the Deck why.
- **Windows Deck to a Windows Pier** works over the same protocol but is
  not yet part of the lab qualification, which covered Linux X11, GNOME
  Wayland and macOS Piers.

## [0.19.1] — not published (included in 0.20.0)

**Exact still image, on by default.** When part of the picture stops
moving, the Pier sends those exact pixels again, losslessly, and the
macOS Deck paints them over the video and checks them against the Pier's
own digest, so a still picture becomes the host's exact pixels rather than
the codec's approximation ([ADR 0016](docs/adr/0016-lossless-refinement.md),
[user guide](docs/user-guide.md#exact-still-image)).

- **Where it engages:** Grading and HDR sessions (HEVC 4:4:4 ten-bit), one
  display, in the stream's own code values — BT.709/sRGB SDR codes for
  Grading, PQ/BT.2020 codes for HDR (verified on the Deck's HDR PQ/EDR
  presenter too):

  | Host | Exact still image |
  | --- | --- |
  | Linux, X11 desktop at depth 30 | Grading and HDR |
  | Linux, Wayland on a compositor with wide screen sharing (GNOME 50 or later, for example the side-by-side GNOME 51 prefix) | Grading and HDR (the wide capture's host read-back is the source) |
  | Windows | Grading and HDR (the FP16 capture is the source; the broker relays the refine lane) |
  | macOS | Grading and HDR (the `xf44` surface VideoToolbox encodes is the source, refined as Y′CbCr 4:4:4 codes and verified to level 2) |

  Auto and Speed, Wayland on GNOME 49 (no wide screen sharing) and every
  other session stream exactly as before, and Stream Statistics says why it
  is off. The Deck paints tiles only over a picture in their own coding
  domain (PQ never over SDR; Y′CbCr codes only through their own matrix and
  range). The macOS host needs a Deck of this release, which accepts
  Y′CbCr tiles; an older Deck is told why it is off.
- **Switches:** the Pier's `video.lossless_refinement` now defaults to
  `enabled` (`"disabled"` turns it off; `validate-config` prints the
  resolved state on Linux and Windows), and the Deck's **Settings › Streaming › Exact still
  image** is on by default. Both must agree; a Deck with it off asks for
  nothing and nothing extra is sent.
- **What it costs:** only spare bandwidth while the picture is still: up to
  half of what the connection has to spare, measured each second from the
  QUIC path, and paused whenever video waits, the path loses packets or its
  queue grows; a part that moves again stays video, and motion is not
  slowed. The video's rate controller no longer lowers the video bitrate
  for queueing that refinement itself causes: refinement pauses for a
  second first, and only queueing that stays is answered (loss always is).
  Before the first path sample the session's video bitrate is the limit. Measured over a local network: a 1080p desktop
  of windows, text and colour ramps is exact and verified in under a
  second, a 4K full-screen photograph in about 6.5 seconds.
- **Proof on the Deck:** Stream Statistics shows *Exact: 100% of picture
  verified (level 3)* once every pixel drawn was checked against the
  host's digest; Lossless Tile View tints refined blocks amber and verified
  ones green.
- It runs on its own QUIC lane (`refine`), granted only to a Deck that asks
  for it; older Decks and Piers are unaffected.

## [0.19.0] — 2026-11-12

**Native tablet ships, proven with a real Wacom Intuos5 on Linux X11, Linux
Wayland and macOS Piers.** A Deck's own pen tablet — the Wacom on the Mac's
desk — is attached to the remote desktop as if it were plugged into the
host. On a Linux Pier the host's own tablet driver takes it, with its
pressure curves and side switches; on a macOS Pier, Arcen reads the pen of a
model it knows (the Intuos5 touch L) and posts tablet events with pressure.
There is **one pen-position path** for every Pier: the Pier reads the pen's
own reports and the Deck draws a pen dot where it points, so the dot follows
the pen instead of the host's smoothed pointer. Measured with an emulated
Intuos5 on an X11 host, one pen report per 41 ms network round trip, the
dot now reaches the Deck a median **16 ms** behind the pen, against **80 ms**
when it followed the X pointer. On a Linux Pier Native tablet is an
administrator opt-in (`usb.native_tablet`, or the installer's
`--native-tablet`), works on X11 desktops from the first connection with no
sign-out, joins native-seat GNOME Wayland desktops, and uses the
distribution's own signed USB/IP controller rather than a self-built kernel
module. The Deck captures the tablet with Apple's IOUSBHost and hands it back
to the Mac when the session ends. Windows Piers keep Tablet support (Windows
Ink).

Also new: the macOS Pier types text and reports Connection Health; Option
keys get an Automatic setting; a user guide for artists and editors; Stream
Statistics; the licence exception, CLA, trademark policy and notice are
final (version 1.0); and official builds carry a published build ID.

What is proven, and how:

- **Real Wacom Intuos5, bridged from a Mac Deck:** first-connection hot-plug,
  drawing with pressure, the Deck's pen dot and hand-back to the Mac, on a
  Rocky Linux 9 X11 Pier, a Rocky Linux 10 GNOME Wayland Pier (native-seat
  desktop) and a macOS Pier, all on a 0.19.0 development build.
- **Latency of the pen dot:** the 16 ms and 80 ms above are medians at the
  Deck, emulated Intuos5, X11 host ([ADR 0023](docs/adr/0023-one-native-tablet-pen-position-path.md)).
- **macOS Pier pressure:** `NSEvent` pressure 0.059 to 1.000 (17 levels in
  the scripted stroke) with the emulated pen, then a real Intuos5.
- **Not proven:** Native tablet on an Intel Deck (no Intel Mac on macOS 13 or
  later with a tablet has run it).

0.19.0 speaks protocol v5, like 0.17.0 and 0.18.0, and every addition is
negotiated: 0.18.0 and 0.17.0 Decks and Piers keep working with 0.19.0,
without the new features. One exception is not negotiated: pen barrel
buttons need an updated Deck and an updated macOS or Windows Pier for both
buttons to arrive as documented (see the upgrade notes).

### Coming from 0.17.0

0.18.0 was not published, so 0.19.0 is the first public release since
0.17.0, and an upgrade from 0.17.0 brings 0.18.0's changes too (listed
under [0.18.0] below). In short: Paste as Keystrokes and Characters mode
type text on any host; Mac keyboards on Windows hosts work the Windows App
way (⌘ as the Windows key, ⌘C as Ctrl+C, AltGr and the ISO key); real cursor
images on X11 and Windows hosts; the colour check; **Test connection** and
`arcen-deck diagnose`; and Connection Health showing the Pier's own
measurement of the network path. 0.18.0's upgrade notes apply too: Wayland
hosts re-qualify on their first start (a few minutes), the Linux installer
adds `/etc/tmpfiles.d/arcen-pier.conf`, and the Windows Pier injects keys by
scan code, so set the Windows user's layout to match the Mac keyboard.

### Upgrade, step by step

1. **Deck:** quit it, replace **Arcen Deck** (`Arcen-Deck-0.19.0-macOS.zip`,
   Apple silicon) or **Arcen Deck Intel** (`Arcen-Deck-Intel-0.19.0-macOS.zip`)
   in Applications, and open it. If you use Native tablet, restart the Mac
   (or turn the Arcen USB helper off and on in System Settings › General ›
   Login Items) so the helper starts on its new socket path.
2. **Linux Pier,** in a maintenance window (a restart ends running sessions):
   ```sh
   chmod +x install-arcen-pier-0.19.0-linux-x86_64          # a download is not executable
   sudo ./install-arcen-pier-0.19.0-linux-x86_64 --check     # what it will do
   sudo ./install-arcen-pier-0.19.0-linux-x86_64 --restart
   ```
   The upgrade keeps `/etc/arcen/pier.json`, including the Native tablet
   choice. To turn Native tablet on, add `--native-tablet` (Rocky
   Linux/RHEL: it installs `kernel-modules-internal`); to turn it off,
   `--no-native-tablet`.
3. **Linux Pier that used `ARCEN_USB_HARD_LAB`:** run step 2 with
   `--native-tablet`, then delete the systemd drop-in that sets
   `ARCEN_USB_HARD_LAB` (`systemctl cat arcen-pier` names it) and run
   `sudo systemctl daemon-reload`. The variable is no longer read.
4. **Linux X11 host with Native tablet for the first time:** restart the
   display manager once while nobody is signed in at the console
   (`sudo systemctl restart gdm`), or reboot.
5. **Windows Pier:** run `install-arcen-pier-0.19.0-windows-x64.exe` in
   PowerShell as Administrator, then reboot.
6. **macOS Pier (preview):** open `ArcenPier-0.19.0.pkg` in Installer.app;
   existing approvals carry over.

### Upgrade notes

- **Linux Pier: Native tablet is an administrator opt-in.** It is in every
  Linux Pier build, and off until `usb.native_tablet` is `"enabled"` in
  `/etc/arcen/pier.json` (`install-arcen-pier --native-tablet` sets it and,
  on Rocky Linux/RHEL, installs `kernel-modules-internal` for the running
  kernel; `--no-native-tablet` turns it off and removes that package if the
  installer installed it). An upgrade keeps the current choice, and the
  installer installs `kernel-modules-internal` only with Native tablet
  enabled. A change needs a Pier restart. See
  [Native tablet](docs/operations/pier-administration.md#native-tablet-usbnative_tablet).
- **`ARCEN_USB_HARD_LAB` is removed.** The 0.18 lab switch
  (`Environment=ARCEN_USB_HARD_LAB=1` in a service drop-in) is no longer
  read, and the `usb-hard-lab` build feature is gone (`native-tablet` is on
  by default). Before upgrading a host that relied on it, run the installer
  with `--native-tablet`, then remove the drop-in.
- Linux (Native tablet on X11): the installer adds
  `/etc/X11/xorg.conf.d/90-arcen-bridged-usb.conf`. An X server that is
  already running (GDM's login screen) reads it only at its next start, so
  restart the display manager (`systemctl restart gdm` while nobody is signed
  in at the console) or reboot once; until then the Pier refuses Native
  tablet on X11 desktops with that reason.
- **Pen barrel buttons with a macOS Deck: update the Deck and the macOS Pier
  together.** The wire's `buttons` now means what it always said (bit 0 the
  first barrel button, bit 1 the second) on both macOS ends. Mixed versions:
  - *Updated Deck, macOS Pier 0.18 or older*: clicks are right (lower barrel
    = right click, upper = middle), but that Pier still copies the wire bits
    into `-[NSEvent buttonMask]`, so an application reading the mask sees the
    **tip** (`0x1`) while the lower barrel is held and the lower side (`0x2`)
    while the upper is held, and no tip bit on strokes.
  - *macOS Deck 0.18 or older, updated macOS Pier*: the lower barrel arrives as
    the second button (a middle click, mask `0x4`) and the upper one is
    dropped, as before; tip strokes now carry the tip bit.
  - *Updated Deck, Linux Pier of any version*: the lower barrel reaches
    `BTN_STYLUS` and the upper one `BTN_STYLUS2`, as documented; with a 0.18
    Deck the lower barrel was the second button and the upper one was lost.
  - *Updated Deck, Windows Pier*: the lower barrel reaches
    `PEN_FLAG_BARREL` on any version. **The upper barrel needs the 0.19.0
    Windows Pier as well:** an older Pier injects it as
    `POINTER_FLAG_SECONDBUTTON`, which Windows ignores for a synthetic pen;
    0.19.0 maps either barrel button to Windows Ink's single barrel
    (`PEN_FLAG_BARREL`).

### Legal

- **Licence exception, CLA, trademarks and notice are final (1.0).**
  [LICENSE-EXCEPTION.md](LICENSE-EXCEPTION.md) (with its REUSE copy
  `LICENSES/LicenseRef-Arcen-Store-Exception.txt`), [CLA.md](CLA.md),
  [TRADEMARKS.md](TRADEMARKS.md) and [NOTICE](NOTICE) are version 1.0,
  effective 2026-11-12. The copyright holder is TUTTIFRUTTI AS (Norwegian
  organisation number 919 650 826). Contributors accept the CLA with a
  statement in their first pull request; it is governed by Norwegian law,
  with Oslo District Court (Oslo tingrett) as the agreed venue. The
  exception grants no rights in third-party components (its section 6):
  the macOS Deck statically links libusb, which stays under the LGPL.
  The exception's list of named stores now includes the Microsoft Azure
  Marketplace beside the Apple App Store, the Mac App Store and the
  Microsoft Store, and still covers any similar store or marketplace.

### Added

- **Native tablet is a product feature.** The Linux Pier, the macOS Pier and
  the Deck build it by default (Cargo feature `native-tablet`). The Linux
  Pier offers it only when the administrator sets `usb.native_tablet =
  "enabled"` (shared `arcen_session::pier_config::NativeTabletPolicy`);
  `validate-config` prints the state and the controller's, `vhci-hcd` is
  loaded at start only when enabled, and a Deck that asks a host which has
  not enabled it is told how to enable it. The release Deck embeds the
  privileged helper built with its IOUSBHost backend; `build-deck-app.sh
  --release` refuses a helper that does not link `IOUSBHost.framework`.
- **The Deck draws the host's pointer where a bridged tablet moves it.** With
  Native tablet the host's tablet driver moves only the host pointer, so a
  Deck drawing its own cursor showed none for the pen. A new negotiated
  record, `CursorPosition` (frame type `0x51`, `cursor_position_v1`, ADR 0014
  amendment 21), lets a Linux Pier report where its pointer is whenever it
  moves for a reason the Deck did not cause (a bridged tablet, an
  application warp) and hides the Mac's cursor over the video; any Mac mouse
  motion takes the cursor back at once. While a bridged pen leads, the Deck
  draws its pen mark there on every host ([ADR 0023](docs/adr/0023-one-native-tablet-pen-position-path.md));
  for an application warp it paints the host's cursor image. The Mac
  pointer is never warped. The Linux and macOS Piers report positions;
  Windows Piers, multi-output Wayland, rotated heads and secondary Deck
  viewports do not yet. The connection dialog recommends
  Local cursor rendering with Native tablet.
- **The Deck hands a captured tablet back like an unplug and replug.** With
  the IOUSBHost backend the helper now resets the tablet while it still
  holds its interfaces, so macOS registers the returning device only after
  the old one has gone; Wacom's driver 6.4.9 had dropped a device that came
  back too quickly, and the tablet stayed dead on the Mac until its agent
  restarted. Proven with a real Intuos5.
- **Native tablet on a macOS Pier** (ADR 0022): the Pier acts as the USB host driver for a Deck's bridged tablet
  over the unchanged URB bridge (shared `arcen_usb_bridge::HidHostAdapter`:
  descriptors checked by the shared guard before anything exists,
  `SET_CONFIGURATION`, `SET_IDLE`, report descriptors, one interrupt IN per
  interface) and the existing entitled `hid-injector` child presents one
  `IOHIDUserDevice` per HID interface, forwarding the host's get/set report
  calls as HID class control transfers. It works on the first connection (no
  sign-out, reconnect or restart), never at the login window. Because such
  devices are system-wide, only pen digitizer and vendor interfaces are
  published (never a keyboard, keypad or mouse, unlike the seat-isolated
  Linux Pier, and nothing on Apple's own vendor pages such as `0xFF01`,
  which macOS turns into Spotlight, Launchpad and brightness keys), and the devices are destroyed the moment the console leaves the
  session's account (Fast User Switch, login window).
  Release Piers carry it and offer it when signed with the virtual-HID
  entitlement (`build-pier-pkg.sh --with-virtual-hid`), gated by that
  entitlement rather than by a configuration key. Pressure: for a model
  whose pen packets Arcen knows (today the Wacom Intuos5 touch L) the Pier
  reads the pen itself and posts CoreGraphics tablet events with pressure,
  tilt, eraser and barrel buttons, the route the tablet maker's own macOS
  driver uses (ADR 0022 §14.1; macOS publishes bridged HID devices as
  `Transport "Virtual"` and the maker's driver does not bind them). It reads the tablet's mode, switches it to Wacom
  mode only inside an authorized attachment and puts the read mode back when
  the attachment ends with the Deck still connected; the Deck's cursor
  follows the pen through host pointer positions (`cursor_positions`,
  relayed by the daemon through the shared `CursorPositionRelay`). Measured
  with the emulated Intuos5: `NSEvent` pressure 0.059 to 1.000 in an
  application window; proven with a real Intuos5. Any other model is refused
  with its ids, and the Deck keeps its tablet (ADR 0022 §14.2). `arcen-pier-macos
  probe-bridged-hid` lists what the HID system sees, round-trips the Wacom
  mode switch and counts reports; because it writes to the device it is
  only in lab builds (`--features lab-bridged-hid-probe`), and shipped
  Piers refuse the subcommand.
- **Emulated Wacom for Native tablet lab proofs** (lab only): a Deck built
  with the new `usb-emulate-lab` feature accepts
  `input-smoke … --usb-emulate wacom-intuos5`, which bridges an emulated
  Wacom Intuos5 touch L (`056a:0317`, "Intuos Pro L" to the Linux `wacom`
  driver; `arcen_usb_bridge::WacomIntuos5`: descriptor
  fixture, control and HID feature-report answers including the `wacom`
  driver's mode switch, and a scripted pen stroke) through the shared USB
  exporter instead of the privileged helper. The Pier sees a captured Wacom.
  `scripts/lab/wacom-replay/verify-pen.py` asserts the strokes' position,
  pressure, proximity and barrel button at evdev on the host. No packaging
  script enables the feature, and `build-deck-app.sh --release` now refuses
  `usb-emulate-lab` and `experimental-raw-hid` (as it did `dev-tools`) and
  any binary carrying the feature's do-not-ship marker.
- **Native tablet on X11 desktops on the first connection**
  ([ADR 0021](docs/adr/0021-bridged-usb-devices-on-their-own-seat.md),
  amended): the bridged tablet stays on `seat0`, and a volatile per-port udev
  rule written before its port connects tags its input nodes for Xorg
  (`ID_INPUT.tags=arcen_bridge,arcen_bridge_<display>`), sets
  `LIBINPUT_IGNORE_DEVICE` and strips `uaccess`. A new installed file,
  `/etc/X11/xorg.conf.d/90-arcen-bridged-usb.conf`, makes every X server
  ignore `arcen_bridge`; each dedicated desktop re-admits only its own tag
  (xf86-input-wacom), so it hot-adds the tablet the moment it connects and
  drops it when the Deck disconnects. No sign-out, no "being prepared", no
  hand-back: the private seat, static devices, parking and state replay are
  removed. The desktop's class turns xf86-input-wacom's
  `PressureRecalibration` off, so a pen that arrives already touching (a
  bridged tablet is read once per network round trip) still presses and
  reports its real pressure; before, drawing pages showed pressure and drew
  nothing. Before a bridge starts, the Pier proves every running X server
  keeps it out (the login screen must have started after the file was
  installed; restart the display manager once after installing) and refuses
  with a reason otherwise; each node is verified from the udev database.
  The session's input isolation classes are unchanged; every desktop ends
  its configuration by ignoring every bridge again and re-admitting only its
  own (a bridged device could imitate Arcen's virtual device names). The
  rule also removes logind's `seat` tag, and tablets with a keyboard
  interface or keyboard keys are refused on X11 (the console keyboard
  handler would take them). A refused or stopped Native tablet now reaches
  the Deck as a consistent result with its reason. Native-seat Wayland
  desktops are unchanged.
- **Native tablet on Linux without self-built kernel modules**
  ([ADR 0020](docs/adr/0020-linux-native-tablet-in-tree-vhci-hcd.md)): the Linux
  Pier presents the bridged tablet on the distribution's in-tree, signed USB/IP
  `vhci-hcd` (a socketpair attached through sysfs; the Deck and the wire are
  unchanged), with the out-of-tree `usb-vhci` kept only as a development
  fallback. With Native tablet enabled, on Rocky Linux/RHEL the installer
  installs `kernel-modules-internal` for the running kernel (GPG checked,
  signer verified, removed again by `--uninstall` or `--no-native-tablet`
  only if it installed it; `--no-native-tablet-modules` opts out). An
  enabled Pier loads `vhci-hcd` once at start; `validate-config` says
  whether it is loaded, or installed but needing a Pier restart (or
  `modprobe vhci-hcd`). After a kernel update the Pier, `validate-config` and
  the refusal name the exact `dnf` command. Native-seat Wayland desktops now accept Native
  tablet: the tablet joins the desktop's own seat and Mutter adds it; headless
  desktops still refuse it.

- **IOUSBHost capture in the Deck's helper** (ADR 0019): an
  `arcen-usb-helper` built with `--features iousbhost` — every release Deck —
  captures the tablet with `IOUSBHost` `DeviceCapture` instead of libusb
  (`--backend libusb` still selects the old path). The helper logs its
  capture backend at start and per capture, and reports it to the Deck in a
  new info frame after the hello; the Deck logs it with the capture. The
  Deck↔Pier wire is unchanged. The helper now reads its whole command line before acting, so
  `--probe --backend libusb` probes with libusb and a bad argument anywhere
  refuses the run.
- **A user guide for artists and editors** ([`docs/user-guide.md`](docs/user-guide.md)):
  one task-ordered manual for the Deck, from installing it to signing out,
  covering saved connections and certificate trust, the four presets and
  what a visible downgrade means, the Colour check, Tablet support and
  Native tablet, the keyboard, clipboard, sound and microphone, displays and
  Retina, the path indicator, reconnecting, and the common refusals. Each
  section names the source files it was checked against. Linked from the
  README and `STRUCTURE.md`.
- **Stream Statistics and Lossless Tile View** (Deck, Session menu, both off
  by default). Stream Statistics is a compact three-line box: the path as the
  Pier measures it (RTT, loss, rate), presented frame rate, codec, chroma,
  depth, served pipeline, SDR or HDR from the transfer (never from bit
  depth), and lossless-refinement coverage. Lossless Tile View tints refined
  16×16 blocks amber and verified ones green. Lossless refinement itself is
  not negotiated by this release, so both report "Refinement off".
- **Lossless refinement, first slice (not enabled)**
  ([ADR 0016](docs/adr/0016-lossless-refinement.md), amended after design
  review): the portable core in `arcen_media::refine` (fences, generations,
  limits, canonical digest, three verification levels), the draft wire
  records and a `refine` lane that a Pier grants only to a Deck that asks for
  it. No product sends refinement yet; existing Decks and Piers are
  unaffected.
- **The macOS Pier types text.** Input-method results, Characters mode and
  Paste as Keystrokes now reach a macOS host as the characters typed: each
  commit goes as Unicode key events (`CGEventKeyboardSetUnicodeString`, at
  most 20 UTF-16 units per event, split by the new shared
  `arcen_input::text::utf16_chunks` without breaking a surrogate pair, an
  emoji sequence, a flag or a combining accent), posted where keys are posted
  and with no modifiers, whatever the Mac's layout. The hello advertises
  `text_commit` and `text_commit_unicode` only where the agent holds the
  Accessibility grant keys need and is not the HID-only login window; an
  invalid commit is refused to the Deck (`text_commit_result`), never typed.
  The Pier's own key-event injection is unchanged. Proven on a Mac: 337 characters (ÆØÅ, emoji with
  skin tone, a ZWJ family, 🇳🇴, e + U+0301) sent over QUIC arrived exactly in
  an AppKit text view.
- **Option keys: a new Automatic setting, the default.** Now that a Mac host
  types text, the Deck's committed-text policy applies to it, and the old
  default (left Option Alt, right Option composes on the Deck) would have
  changed how a Mac host receives right Option. Automatic keeps both Option
  keys as Option for a Mac host, as in 0.18.0, and is left Alt / right
  characters for Windows and Linux hosts, as before; input-method text,
  the characters Characters mode sends and Paste as Keystrokes still arrive
  as text (an Option chord stays a key, typed by the host's layout). Settings →
  Input → Typing lists Automatic first; a mode chosen there applies to every
  host. A `settings.json` from an older Deck keeps a chosen Both Alt or Both
  characters, and reads its stored default as Automatic (older Decks saved
  the default with every save). Shared:
  `arcen_input::text::OptionKeyPreference` (`shared/input/src/text/policy.rs`),
  resolved against the host platform from the hello like the keyboard
  shortcut mapping.
- **The macOS Pier sends `health_stats`**, so the Deck's Connection Health
  shows its delivery and the **Path** row (RTT, loss, rate, controller) as
  the Linux and Windows Piers do. Windows are built by the new shared
  `arcen_telemetry::HostWindowTracker`, the path by
  `arcen_transport::PathQualityTracker`; in the installed split service the
  network service, which owns the QUIC connection, fills the path into the
  agent's message as it relays it. A still desktop is judged against what
  ScreenCaptureKit offered, so idling is not reported as a fault, while a
  window with nothing sent at all (keepalive included) reads as critical.

- **`ARCEN_QUIC_MTU_DISCOVERY=on`** (Pier and Deck) lets QUIC probe for
  packets above 1200 bytes, up to 1452, with black-hole fallback to 1200. It
  is off by default: over a 1280-byte IPsec VPN the tunnel fragmented the
  probes, so they succeeded, and full-rate data at that size was then lost in
  bursts every minute (about 6 % less goodput). The host now reports the MTU
  it sends with (`HealthStatsMsg.path.mtu_bytes`, additive, with
  `black_holes`), the Deck's Connection Health `Path:` row shows it, and
  `arcen-deck diagnose` reports the packet size its own connection reached.
- **Licensing and official builds.** Arcen is owned by TUTTIFRUTTI AS and
  licensed `AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception`: the
  [Arcen Store Distribution Exception](LICENSE-EXCEPTION.md) additionally lets
  anyone offer it free of charge through an app store, provided the source is
  public and the Arcen names are not used. New: [NOTICE](NOTICE) (official
  distribution), [TRADEMARKS.md](TRADEMARKS.md), a
  [Contributor License Agreement](CLA.md) (licence grant, not assignment;
  signed manually, recorded by TUTTIFRUTTI AS), [CONTRIBUTING.md](CONTRIBUTING.md)
  and [CONTRIBUTORS.md](CONTRIBUTORS.md). Every first-party source file carries
  an SPDX header, `REUSE.toml` covers the rest and records the true licences of
  vendored code, and `reuse lint` passes. The legal texts are final
  (version 1.0, effective 2026-11-12; see Legal). The canonical public repository is
  https://github.com/Aanerud/arcen_public; every source offer names it.
- **Official build IDs.** Official builds are stamped from the published
  release tag (`scripts/release/official-build-id.sh`,
  `official-<UTC>-v<version>-<commit>`); see
  [Telling an official build apart](docs/operations/versioning-and-releases.md#telling-an-official-build-apart).

- Lab builds of the macOS Pier (`--features lab-pen-probe`, enabled by no
  packaging or release script, so shipped Piers do not contain it):
  `arcen-pier-macos probe-pen [--area X,Y,W,H] [--strokes]` posts a
  Tablet support pen through the session input path (`InputSession::apply`,
  the call a live session makes for each `pen_event`) and reads back what
  `AppKit` delivered with a global `NSEvent` monitor — proximity and
  capability mask for pen and eraser, tilt, rotation, and with `--strokes`
  pressure and the pen button mask. Hovers only by default. Run it from the
  Aqua session (posting needs the responsible app's Accessibility approval).
- `arcen-pier-macos probe-pen-hid`: presents a generic HID pen digitizer
  through the `com.apple.developer.hid.virtual.device` entitlement, hovers it
  (never presses) and reports what macOS delivered. On macOS 26.7.2 the
  answer is that proximity arrives and pressure, tilt and rotation do not,
  which is why Tablet support on a macOS Pier stays on CoreGraphics tablet
  events rather than a virtual digitizer. The descriptor and report packer are
  shared (`arcen_input::hid_digitizer`) for a future Linux `uhid` pen.
- **Native tablet: the USB exporter engine is shared** (`arcen-usb-bridge`
  `UsbExporter` behind a `CapturedUsbDevice` backend trait, ADR 0019 step
  M1): URB admission, cancellation tombstones, locally answered standard
  requests, the interrupt hold, status mapping and exactly-once release are
  one portable engine, ready for an IOUSBHost backend. The macOS USB helper
  runs on it through a libusb backend, proven with synthetic tests only; the
  physical tablet was later proven through the IOUSBHost backend. A synthetic backend over the lab tablet
  drives the engine end to end in `cargo test` on any OS (golden
  enumeration, held polls, cancellation races) and proves every session exit
  releases the device exactly once. No wire change; the one behaviour change
  is that a transfer whose thread panics now completes as `io` instead of
  never.
- **Native tablet: the IOUSBHost capture backend**
  (`clients/macos/usb-iousbhost`, ADR 0019 step M2 part one): captures the
  classified device with `DeviceCapture`, configures it with
  `matchInterfaces: NO`, opens its interfaces and interrupt pipes, runs every
  transfer asynchronously on one serial queue, releases on a dedicated thread,
  surrenders the device when another client asks for it, and records a
  root-owned capture marker so a restarted helper resets a device a crash
  left captured. Apple's frameworks only, through `objc2-io-usb-host`; no
  libusb. The helper links it with the `iousbhost` cargo feature, which
  release Decks enable from 0.19.0 (see above); a real Intuos5 has since
  been captured, bridged and handed back with it.

- **Native tablet on GNOME Wayland desktops.** A native-seat Wayland desktop
  (one that runs its compositor on its own logind seat, `wayland.session_mode`)
  takes the bridged tablet on that seat, where Mutter adds it like a tablet
  plugged in there; headless desktops refuse it. GNOME hides its pointer
  while a pen is near the tablet, so the Deck draws a small pen dot where the
  pen points. GNOME applies its own stylus pressure curve (see Known
  limitations).
- Lab: `--usb-emulate-stroke realistic` for the emulated Intuos5 (in-range
  packets, the Grip Pen's real tool id, a landing already pressing, tilt),
  and `tilted` (the scripted stroke sweeping both tilt axes); a lab Deck
  also plays them in the GUI (`--connect HOST PORT … --usb-emulate`).
- Lab: `input-smoke --pen-file PATH [--pen-go-file PATH]` sends pen samples
  from a file, optionally only once a lab script writes `go`.

### Changed

- **Native tablet: one path for where the pen is, and one pen dot.** On an
  X11 desktop the Deck's pointer for a bridged pen followed the X pointer,
  read every 8 ms after `xf86-input-wacom` had processed and averaged the
  report, so it trailed the pen (median about 64 ms behind it when sent, at
  one pen report per 41 ms round trip, measured with an emulated Intuos5).
  The Linux Pier now reads the bridged pen's own reports on X11 as it
  already did on Wayland, one shared path for every Pier (ADR 0023), and
  sends each position as it arrives (median 0.2 ms after the report). The
  Deck draws the same white pen dot on X11, Wayland and macOS hosts. An X11
  tablet the user remapped (tablet area, rotation, transformation matrix,
  mouse mode) falls back to the X pointer.
- **macOS Pier: Native tablet accepts only a tablet whose pen it reads.**
  Any other model is refused with its ids and the Deck keeps its tablet,
  instead of a half-working pointer without pressure (ADR 0022 §14.2).
- macOS Deck: Settings › Input › Tablet says which saved connection the
  tablet mode is remembered for (each saved connection keeps its own).
- **`bbr-guarded` now engages when it should.** Its loss ceiling ignored any
  round QUIC called application-limited, and Quinn calls a video sender that
  between frames. The cap now follows lossy round trips whatever the
  application did, and is removed only when the bytes in flight stayed below
  half of it. At an 8 Mbit/s shallow bottleneck it loses 12 % instead of
  BBR's 54 %, with frame age p50/p95 0.77/1.06 s instead of 1.36/1.89 s;
  under 1 % random loss it matches BBR. `bbr` remains the default.
- **A still desktop no longer reads as degraded.** Every capture path sends a
  frame only when the screen changes, plus a keepalive a second, yet health
  judged the frame rate against the target alone: a macOS Pier serving an
  idle desktop at 8–9 fps of a 30 fps target logged `degraded`, and the Deck
  raised `HEALTH_CRITICAL` (fps 15 against a threshold of 21) on sessions that
  delivered every frame. Health now judges frame rate against the content
  that changed (`fps_content`, new in `HEALTH_SNAPSHOT`) wherever the capture
  path can prove it for the whole health window — macOS capture counts in a
  window where `ScreenCaptureKit` reported idle buffers or Keel skipped an
  unchanged frame, every Linux
  capenc second classified `no_damage` or with unchanged NvFBC grabs, every
  Windows capenc second with idle polls, and on the Deck and in each Pier's
  view of the client the frames received. Falling behind changing content
  (including a Detail rate cap or back-pressure below a busy screen), drops,
  and windows that sent nothing are still flagged; unproven windows are judged against the
  target as before. One decoded frame in
  flight at a window boundary no longer counts as `unpresented`, which is
  judged only on windows of at least ten decoded frames (a keepalive cannot
  be superseded). The policy
  lives once in `arcen-telemetry` (`fps_expected`, `fps_threshold`,
  `unpresented_basis_points`); `HostWindowTracker` now reports what the
  source offered as `fps_content` rather than lowering `fps_target`, and the
  macOS Pier's `health_stats` relies on the shared stall rule. A macOS
  Detail stream now paces from the previous sent frame's arrival with half a
  frame of margin, so on-cadence frames are no longer skipped and a send
  blocked by back-pressure no longer makes the next frames look early.
- **A Windows Pier keeps a slow network's backlog where it can see it.** Its
  broker's lane relay held up to 16 encoded frames (plus the lane's own
  queue) in front of a slow path, then dropped them and asked for a
  keyframe, so pictures aged and keyframes stormed. The relay now reports
  every frame it releases to the desktop agent, which keeps one frame in
  flight and holds the rest in the shared budgeted send queue the Linux Pier
  uses: the encoder stalls and its next capture is the current screen. At an
  8 Mbit/s cap: frame age p95 ~1.58 → ~1.34 s at the same median and frame
  rate, stutters ~259 → ~37 per minute, and no keyframe storms (~116 → 0
  keyframe requests per run); a clean link is unchanged.
  `ARCEN_VIDEO_BACKLOG=legacy` on the service restores the old behaviour in
  full: the service hands the choice to each session agent, which runs with
  the signed-in user's environment, not the service's.
- **Every Grading or HDR refusal from a Windows Pier names its hardware
  cause** and leads with a code from the
  [error-code reference](docs/operations/error-codes.md#grading-and-hdr-refusals):
  every GPU encoder session in use, no usable GPU encoder (device, driver or
  vGPU licence), the NVENC runtime missing, a ten-bit format the GPU cannot
  encode, a display that cannot give a ten-bit (FP16) capture surface, or a
  host with only the software encoder. Before, it said only that software
  is 8-bit, or blamed an administrator video pin. The Deck shows the reason
  at sign-in and in the Colour check.
- A Windows Pier's Native tablet refusal says the mode is not offered on
  Windows hosts (ADR 0017) instead of "not yet".

- **Pen input looks forward: Wintab is not supported, and Windows hosts do not
  offer Native tablet** ([ADR 0017](docs/adr/0017-forward-only-pen-input.md)).
  Windows hosts deliver pens through Windows Ink only (the typed pen, Tablet
  support); set drawing applications to Windows Ink. Native tablet (USB
  bridged) on Windows would need a signed kernel driver the project does not
  fund; it is offered by Linux Piers (administrator opt-in) and macOS Piers,
  whose route is through Apple entitlements rather than a driver.
  No behaviour changes: a Windows Pier already refused Native tablet with a
  reason.
- **Linux and macOS Grading say `srgb` too.** Xorg's depth-30 desktop and
  the macOS Pier's `ScreenCaptureKit` surface already carried the desktop's
  own sRGB code values; for a Deck that presents `srgb` (0.18.0 and later)
  the stream, READY/hello and session events now state the `srgb` transfer
  (H.273 13) instead of BT.709, and the Colour check reads "served srgb" on
  every Pier. Older Decks, eight-bit sessions and administrator-pinned
  variants keep `bt709`. One shared rule decides it on all three Piers
  (`arcen_media::video::serve_sdr_desktop`).
- **Wayland Grading gives a 0.18.0 or later Deck the desktop's own codes and
  says `srgb`.** A Wayland Grading session converts the compositor's HDR (PQ)
  composition to SDR; it used the BT.709 curve and stated `bt709`, which
  left the desktop's dark greys up to 20/255 low. For a Deck that claims
  `srgb_transfer` the Linux Pier now passes `transfer=srgb` to capenc, whose
  shared capture admission re-encodes with the inverse of the curve Mutter
  decoded the desktop with — a pure 2.2 power, measured on GNOME 51.1, not
  the sRGB curve, which would still leave shadows up to 8.5/255 low
  (`rgb10-pq-to-g22`, `f16-pq-to-g22`) — and states `srgb` in ADMISSION,
  READY, the hello and the SPS (VUI 13). Older Decks, HDR and every
  eight-bit route are unchanged.

- **Linux Wayland Pier re-proves X11 applications itself after
  `/tmp/.X11-unix` is repaired.** When the qualification record skipped X11
  applications because the directory was unsafe and start-up finds it sound
  again (`x11_precondition_cleared`), the Pier re-proves X11 before it
  serves, once per boot (`capture.wayland_auto_requalify`, default on): a
  short run of the automatic re-qualification (one display, one codec) whose
  X11 proof alone replaces the record's skipped one; every other fact of the
  record is kept, and a precondition that failed again leaves the record
  unchanged. It never refuses start, logs a typed `x11_requalify_outcome`
  (`x11_apps_proven`, `x11_apps_failed`, `precondition_returned`,
  `not_recorded`) and records it in `<record stem>-x11-requalify.json` and
  the record's new optional `automatic_requalification` section (older
  builds ignore it; unknown values in it read as `unknown`).
  `arcen-pier repair-x11-unix-dir` re-arms it after a repair; the new
  `--requalify` also restarts `arcen-pier` to run it now (off by default: a
  restart ends running desktops), and `--record PATH` names a
  non-production record.

### Known limitations

- **Application ink on X11 follows `xf86-input-wacom`'s smoothed pointer.**
  The Deck's pen dot follows the pen's own reports, but the X driver averages
  several samples before it moves the pointer, and a bridged tablet delivers
  one report per network round trip, so ink in X11 applications trails the
  dot on a slow link.
- **GNOME Wayland draws thinner lines with Native tablet.** Mutter squares
  stylus pressure on its default curve, so a light stroke reaches
  applications lighter than on X11. Raise the stylus pressure curve in GNOME
  Settings › Wacom Tablet (the session user's own setting).
- **A held side switch looks like "pen but no ink".** If the pen's upper
  side switch is held (or a worn switch reports it held), GNOME on X11
  treats it as a held right button: pressure shows, nothing draws. Lift the
  pen out of range and back, or remap the switch in the desktop's tablet
  settings.
- **Safari reports every pen as a mouse.** WebKit on macOS gives pages
  `pointerType: "mouse"` and no pressure for any pen, local or remote; test
  pen pages on a macOS Pier in Chrome or Firefox.
- **A macOS Pier refuses tablet models it does not know.** It reads the pen
  only of a model whose packets Arcen knows (today the Wacom Intuos5 touch
  L); any other tablet is refused with its ids, and the Deck keeps it and
  can use Tablet support. Touch and ExpressKeys do not reach a macOS Pier.
- **A tablet remapped in GNOME Settings offsets the Deck's pen dot** on a
  Wayland desktop: the Pier assumes GNOME's default mapping. Keep the
  default mapping, or use Use host cursor.
- **No pen positions on multi-monitor Wayland.** A multi-output Wayland
  desktop, Windows Piers and secondary Deck viewports do not report host
  pointer positions yet.
- **The Linux Pier accepts pen events in mouse compatibility mode.** A Deck
  that negotiated mouse compatibility can still send typed pen events and
  the Linux Pier injects them; the session is the user's own, so this is a
  hardening follow-up.
- **Native tablet on an Intel Deck is untested on hardware.** The Intel Deck
  carries the same helper and IOUSBHost backend, and Native tablet needs
  macOS 13 on the Deck.

### Removed

- The `usb-hard-lab` build feature (use `native-tablet`, on by default; the
  lab-only `usb-emulate-lab` Deck feature stays), and the 0.18
  `ARCEN_USB_HARD_LAB=1` service environment, which 0.19.0 no longer reads:
  set `usb.native_tablet = "enabled"` (`install-arcen-pier --native-tablet`)
  on a host that relied on it, then remove the drop-in.
- The planned clean-room `wintab32.dll` for Wintab-only Windows applications
  is withdrawn from the roadmap, and the Windows Native tablet driver (ADR
  0012) is no longer planned.

### Fixed

- **Native tablet: the pen's pointer is always visible on the Deck, and the
  Mac's own arrow stays hidden.** On GNOME Wayland the host hides its
  pointer while the pen is near the tablet, so the Deck drew nothing where
  the pen was; it now draws a small pen mark there (on every host, ADR 0023). With a real tablet the
  Mac's arrow could stay on screen beside the pen's pointer, because the
  Deck judged where the Mac pointer was from the last mouse event its
  window had seen — and a bridged tablet never moves the Mac pointer after
  the session window has covered the screen, so a user working only with pen and
  keyboard never gave it one. The Deck now asks macOS where
  its pointer is, whether Arcen is the active application, and which
  window is on top there, so it hides the arrow only over its own video and
  never over another application.

- **Linux Wayland, Native tablet: the Deck's cursor follows the bridged
  pen.** Mutter keeps a tablet tool's position apart from the core pointer,
  and its `ScreenCast` cursor metadata reports only the core pointer, so a
  Deck with a Local cursor never saw the pen move (measured on Rocky 10,
  Mutter 49.4: no `CursorPosition` records during whole emulated strokes).
  On a native-seat desktop the Pier now reads the bridged pen's own evdev
  node (read-only, never grabbed) and reports where GNOME's default mapping
  puts it (shared `arcen_input::tablet_pointer`); when the pen leaves, the
  core pointer's position is reported again. Lab Decks gained
  `--usb-emulate-stroke landing|alternate` (shared
  `arcen_usb_bridge::intuos5_landing_stroke`) to replay a pen that lands
  already pressing.

- **USB bridge records no longer share the clipboard's relay queue**
  (`arcen_transport::quic::PierLaneRelay`): a clipboard transfer could fill
  the 64-record bulk backlog and drop a Native tablet URB submit, silencing
  that interface. USB bridge records now have their own queue
  (`RELAY_USB_BACKLOG`), drained first, with `usb_records`/`usb_dropped`
  counters; the relay also ends with `ConnectionLost` instead of panicking
  when every lane task has stopped.
- **The shared bridged-device guard checks every identity field a short
  device descriptor carries** (a 14-17-byte answer skipped the check), and
  the HID host adapter requires a complete 18-byte device descriptor and
  9-byte configuration header (a short one panicked the session). An
  interrupt transfer silent for 10 s is cancelled and renewed; its tombstone
  lasts until the Deck's Cancelled answer and briefly after it, so a report
  that races the renewal, in either order, is delivered rather than ending
  Native tablet.
- **Windows Pier pen: eraser, second barrel button and displays left of the
  primary** (`hosts/windows/src/input.rs`). An eraser end hovering over a
  Windows host clicked and drew, and applications never saw an eraser: the
  Pier set `PEN_FLAG_ERASER`, Windows' eraser *contact*, for the whole time
  the eraser was in range. It now sets `PEN_FLAG_INVERTED` in range and adds
  `PEN_FLAG_ERASER` only in contact, as a real digitizer does. The upper
  barrel button did nothing (Windows ignores an injected
  `POINTER_FLAG_SECONDBUTTON` for a pen); both barrel buttons now press the
  single Windows Ink barrel. A pen on a Deck display placed left of or above
  the primary landed on the far edge of the primary: pen coordinates are now
  relative to the virtual screen, as `InjectSyntheticPointerInput` requires.
  Proven on the Windows lab with a host-side `WM_POINTER` probe.
- **Windows Pier: the first sign-in after a sign-out no longer touches the
  display before the user's desktop exists** (`hosts/windows/src/session.rs`).
  The session agent starts while LogonUI still owns the new session's input
  desktop, and Windows refuses display changes from it (`SetDisplayConfig`
  returns ERROR_ACCESS_DENIED). The agent changed the display at once: a
  recovery journal left by a signed-out session could not be applied and was
  set aside as `display-recovery.unrestorable.*.json`, and on a slow first
  sign-in (Windows' "almost done setting up your PC" screen) NVIDIA headless
  provisioning could fail in the same window, and the Deck was told the host
  "could not start the session". Windows now waits for the user's Default
  input desktop, the evidence capture already waited for, before display
  restoration or provisioning: the pending journal is restored after a
  short wait instead of being set aside.

- **Pen barrel buttons and the tablet button mask between a macOS Deck and
  every Pier.** The macOS Deck put AppKit's barrel bits on the wire unshifted
  (lower side as bit 1, upper side as bit 2), while the wire and every host
  read bit 0 as the first barrel button: the lower button arrived as the
  second (a middle click on a macOS Pier) and the upper one was dropped. The
  Deck now sends the lower button as bit 0 and the upper as bit 1. The macOS
  Pier, for its part, copied the wire's bits into `kCGTabletEventPointButtons`,
  which `-[NSEvent buttonMask]` returns as is: a tip-down stroke read
  `buttonMask` 0 and the first barrel button read as the tip. It now sets
  AppKit's layout — tip contact bit 0, barrel buttons bits 1 and 2. A 0.18 or
  older macOS Deck still sends the shifted bits to an updated Pier.
- **Linux Pier: an absent tablet is no longer reported as a missing USB
  backend.** A Deck whose helper captured nothing (no tablet attached, or the
  helper refused it) asks for Native tablet without a device and with
  `usb_hard_v1: false`; a host that has the backend now answers "Native tablet
  (USB bridged) refused: the Deck did not capture a USB device. Use Tablet
  support instead." instead of "…needs the USB virtualization backend…".
  A host that has not enabled Native tablet now says so and how to enable
  it; a host without a controller names what is missing.

- **Security: the macOS USB helper's socket no longer lives in `/var/run`.**
  The helper set the socket's mode and owner with `chmod`/`chown` on its path
  after `bind`; `/var/run` on macOS is writable by group `daemon`, so a
  process in that group could race a symbolic link onto the path and have the
  root helper change an arbitrary file's mode and owner. The socket is now
  `/private/var/db/arcen/run/usb-helper.sock`, in a directory the helper
  verifies from `/` down (root-owned, writable by nobody else, no links) and
  pins by descriptor; mode and owner are set with `fchmodat`/`fchownat`
  without following links, under a 0077 umask. The Deck now refuses a helper
  that is not running as root (`getpeereid`). After updating the Deck,
  restart the Mac (or turn the helper off and on in Login Items) so launchd
  starts the helper on the new path.

- **macOS Pier: the pen's pressure reaches applications.** Applications read
  a tablet's pressure from `-[NSEvent pressure]`, which macOS takes from the
  mouse-pressure field the Pier never set, so every stroke from a Deck read
  as full pressure (1.0) and every hover as none. The Pier now sets it beside
  the tablet field. Proximity is now posted as a real tablet-proximity event
  carrying a capability mask, so `tabletProximity:` runs, applications learn
  when the pen or the eraser arrives, and toolkits that size their tablet
  support from the mask see pressure, tilt and rotation. Measured with an
  AppKit window on macOS 26.7 against the Pier's own stream and input path:
  before, pressure 1.000 on every press and drag and no proximity event;
  after, the sent pressures (0.25, 0.90, eraser 0.50), tilt, rotation,
  barrel button and both ends of the pen.
- macOS Pier: at the login window, where a pen can only drive the virtual
  pointer, the hello no longer claims pressure, tilt, rotation, eraser or
  proximity.
- **Every Pier's start-up line names its build.** The Linux Pier's
  `arcen-host starting` and the Windows Pier's `QUIC direct-session endpoint
  listening` now carry `build_id`, `revision` and `profile` beside the
  version, and the macOS Pier prints `arcen-pier-macos <version> build <id>
  rev <revision> (<profile>)` before it listens, from the same shared
  `arcen_protocol::build_identity` its hello advertises; a host log answers
  "which build was serving" the way the Deck's log already does.
- **The Linux installer says the host's speakers are now muted.** Upgrading
  a Pier older than 0.17.0 (asked with `--version`, as `nobody`, before it is
  replaced) on a configuration that does not name `audio.local_playback`
  prints a NOTE that sessions now mute the host's speakers and how to keep
  them playing; before, only the changelog and the administration guide said
  so.
- **Linux installer and Pier messages say what happened.** Every
  qualification command the installer prints (an upgrade under a running
  Wayland Pier, `--no-service`, a failed qualification, a dry run) now
  carries the `--qualify-output` sizes it was given as `--output`, and the
  upgrade note says those sizes are not in the record yet, because the
  automatic qualification at the next start measures only the known ones.
  An upgrade on a host whose firewall already allows 18444/udp says
  `18444/udp was already open` instead of "opened" beside firewalld's
  `ALREADY_ENABLED` warning; the installer asks firewalld and ufw only for
  what needs changing. capenc that was killed or exited with an error during
  a stop is logged as a warning with its exit status, never as
  `capenc exited cleanly`.
- Linux Pier tests: the stale-display tests no longer bind and drop a
  listening socket to stand for a dead Xorg's; a process another test forked
  meanwhile could hold the listener until it exec'd, so the display read as
  serving clients. The socket is now bound and never listened on.
- Linux Pier tests: four more load-dependent failures are gone. The fake
  `pkaction`/`pkcheck` and `nvidia-settings` tools are written by a child
  shell, so the test process never holds a writable descriptor that a
  concurrently forked child could inherit (ETXTBSY: polkit read `Unknown` or
  exit -1); the `nvidia-settings` stub is no longer probed by running it,
  which left an empty argument record that the drop-guard test read as the
  restore's, and it renames its record into place; the lane test waits for
  the video backlog instead of a fixed 200 ms. The lease test's fake `pactl`
  is written the same way, and it waits for the lease's subscription itself
  rather than a fixed 800 ms.
- **A Linux host copy reaches the Deck once.** Duplicate host clipboard
  notifications are filtered on Linux as on Windows: one desktop copy could
  reach the Deck twice. The Linux Pier now
  passes every host item from the Xorg clipboard child and from the Wayland
  session agent through the shared `RepeatedCopyFilter` before it reaches the
  Deck, and an item the Deck sends clears it, as on Windows.
- **Linux Wayland Pier: a desktop ended outside Arcen gives its native seat
  back at once.** After `loginctl terminate-session` (or any end the Pier did
  not start) the retained desktop, and a native desktop's own seat with it
  (`vkms`, udev rules, the GPU's card node), stayed until the next sign-in or
  a Pier restart. The Pier now watches logind's `SessionRemoved` for the
  desktop's session and tears it down at once, or as soon as a Deck still
  attached to it goes. Measured on Rocky 10.2: the seat was taken apart 6 s
  after `terminate-session` with nobody attached, and 6 s after it with a
  Deck attached (the Deck was told the desktop session ended).
- **A declared Rec.2100 PQ Xorg desktop served as Grading** is converted to
  SDR on the sRGB curve for a Deck that presents `srgb`, so an sRGB window
  composed into PQ comes back with its own code values; the BT.709 OETF it
  used before moved the shadows by up to 16/255.
- **A Windows Pier hands a crashed Deck's session to the same user at once.**
  When a Deck died mid-session (process killed, network gone), its
  connection stayed open until the QUIC idle timeout and then moved into the
  resume hold, so an immediate sign-in by the same account waited 30 s for
  the display and was refused; only the attempt after that worked. The new
  sign-in now proves the old connection silent (3 s, as on Linux), closes it,
  and the old attachment hands the desktop over instead of holding it for a
  resume; a detached attachment that is still holding gives way the same
  way. Measured on the lab host: refused after ~34 s before; first picture
  ~18 s after the kill now (an ordinary sign-in takes ~11.5 s). A Deck of the
  same account that is still answering keeps the display; another account is
  still refused. The same cause made quick back-to-back sign-ins with resume
  enabled occasionally time out (when a Deck exited without its close
  reaching the host); in a repeated comparison every attempt now succeeds.
- **A Windows host copy reaches the Deck once.** One copy can raise two or
  three clipboard notifications on Windows (.NET `Clipboard.SetText` places a
  data object and then flushes it under a new clipboard sequence number), and
  each was sent, so a 1.3 MB item crossed the bulk lane up to three times. The
  Pier now drops a local copy identical to the one it sent in the previous
  2 s (shared `arcen_media::clipboard::RepeatedCopyFilter`, which keeps a
  keyed fingerprint, never the content); copying the same text again later,
  or after the Deck has sent something, still goes through.
- The Windows installer's `--uninstall` (and `--purge`) no longer leaves an
  empty `C:\Program Files\Arcen` behind: once the Pier's prefix is gone, the
  `Arcen` directory above it is removed when nothing else is in it. A custom
  `--prefix` outside an `Arcen` directory never has its parent touched.
- **Every Pier wipes a sign-in's password on every refusal path.** The shared
  `AuthResponse` now wipes its password, resume holder nonce and resume grant
  whenever it is dropped, so a Windows or macOS Pier that refuses a sign-in
  before authenticating it (display plan, video request, disclaimer) no
  longer leaves the password in freed memory. The Windows Pier also wipes the
  raw `auth_response` frame and the copy it strips before starting the
  session agent (it was cleared, not wiped); the macOS Pier carries the
  password in a wiping buffer up to PAM and wipes its PAM conversation copy.

## [0.18.0] — 2026-11-09

A Mac keyboard now works on a Windows host the way Windows App makes it work —
⌘ as the Windows key, ⌘C as Ctrl+C, AltGr and the ISO key on European
layouts — and Windows and Linux Piers type text by character: **Paste as Keystrokes**
types the Mac clipboard where clipboard sharing cannot reach, and
**Characters** mode sends what was typed whatever the host's layout. On
GNOME Wayland that text is typed only by key strokes the session's keymap
proves. With the Mac's own cursor, X11 and Windows hosts now show the
application's real cursor images. For colour work, the new **Colour check**
measures every decoded pixel against a known pattern; with it, Windows
Grading now carries the desktop's own sRGB code values exactly, Linux Grading
is served at a Retina Mac's full size, and Windows HDR at Retina size no
longer ends the session. **Test connection** and `arcen-deck diagnose` check
the way to a Pier without a password, and an operations reference lists
every `pier.json` key and every error code. Connection Health shows the
Pier's own measurement of the network path, and the Linux Pier no longer
lets encoded frames pile up behind a slow network.

0.18.0 still speaks protocol v5, and every addition is negotiated: a 0.17.0
Deck and a 0.17.0 Pier keep working with 0.18.0, without the new features.

### Upgrade notes

- Linux Pier: the installer adds `/etc/tmpfiles.d/arcen-pier.conf`, which keeps
  `/tmp/.X11-unix` and `/tmp/.ICE-unix` out of systemd's daily `/tmp` clean
  (uninstall removes it). A host whose `/tmp/.X11-unix` is owned by a user
  serves desktops without X11 applications until an administrator runs
  `arcen-pier repair-x11-unix-dir` and re-qualifies. Going back to an earlier
  build after such a qualification needs a re-qualification.
- **Windows Grading is served in the sRGB curve to 0.18.0 Decks.** The
  stream and session report the `srgb` transfer (H.273 13) instead of
  BT.709; tools reading a Windows Pier's session log or a stream's colour
  tags for Grading see the new value. Older Decks keep the BT.709 encoding.
  Update the Deck to get the desktop's own colours.
- **`ARCEN_QUIC_CONGESTION`** accepts `bbr` (default), `cubic`, `newreno` and
  `bbr-guarded` (experimental, not recommended yet). An unknown value keeps
  the default.
- **Linux video backlog.** The Linux Pier stops taking frames from the
  encoder while two encoded frames wait for the network;
  `ARCEN_VIDEO_BACKLOG=legacy` restores the 0.17.0 queue.
- **Wayland hosts re-qualify after the upgrade.** The new Pier executable
  makes the qualification record stale, so the first start re-qualifies the
  host (a few minutes) before it serves, as `capture.wayland_auto_requalify`
  allows by default.
- **Windows keys are injected by scan code**, so the Windows session's own
  layout decides each character, as for a local keyboard: set the user's
  Windows layout to match the Mac keyboard (for example Norwegian for a
  Norwegian Mac keyboard).

### Added

- **Paste as Keystrokes.** **Connection → Paste as Keystrokes** (⌃⌥⌘V) types
  the Mac clipboard's plain text into the host as keystrokes, for the places
  clipboard sharing cannot reach: the Windows sign-in, lock and elevation
  screens, a Linux lock screen, a virtual-machine or firmware console. It
  releases every key the shortcut held on the host first and waits until no
  modifier is held, types one character at a time at a steady 25 a second
  (Settings → Input → Paste as Keystrokes: 10–250; Windows 11 Notepad dropped
  keys at 50), asks before more than 2,000
  characters (configurable) and never types more than 100,000, stops between
  two characters on Esc or **Stop Typing**, shows its progress, sends line
  breaks as Return and tabs as Tab, and leaves out a line break at the very
  end so a copied password never submits a form by itself. The text is never
  logged by Deck or Pier — only counts — and characters a host could not type
  are counted, never quoted. A host whose clipboard policy keeps this Mac's
  clipboard out refuses it too; a host with no text route says so instead of
  receiving key positions a different layout would mistype. The pacing,
  bounds and character classification are shared
  (`arcen_input::text::paste`).
- **Keyboard: Characters (⌃⌘U) and Physical Keys (⌃⌘K).** The Connection menu
  switches a session between key positions and characters with Windows App's
  chords; a saved connection can start in either (Edit connection → Keyboard
  mode, `keyboard_mode`). In Characters mode every printable character —
  dead-key accents, Option characters, input methods — arrives as typed on
  the Mac whatever the host's layout, while shortcuts, navigation and
  function keys stay keys; a badge over the video says so.
- **Windows and Linux Xorg hosts type committed text.** Both now advertise
  `text_commit` (the Windows Pier typed it but never offered it). Every Pier
  types each character by its key on the host's own layout when that layout
  provably has it (Windows: `VkKeyScanExW` + `ToUnicodeEx` with Caps Lock as
  it is; Xorg: the server's keymap and active group) and no key is held, and
  otherwise by its text route: Windows Unicode input, or on Xorg an unused key
  (F13–F24 or a Japanese key, never one the kernel or logind acts on) briefly
  mapped to the character. So the Mac's input method, Characters mode and
  Paste as Keystrokes reach Windows and Xorg desktops too; the rule is shared
  (`arcen_input::text::route_char`).

- **Mac keyboards on Windows hosts work the Windows App way.** The Deck now
  learns the host's platform from its hello (new `server_hello.platform`,
  falling back to `session_type` and the build identity, so 0.17.0 Piers are
  recognised too) instead of treating every host as Linux. On a Windows host
  ⌘ is the Windows key — ⌘ alone opens Start, ⌘E opens File Explorer — while
  ⌘C, ⌘X, ⌘V, ⌘A, ⌘Z, ⌘F and ⌘W are sent as Ctrl with that key, never as
  Windows+Ctrl; ⌘ with a click is Windows+click and never opens Start (the
  Windows Pier masks that release with an invisible key; older Windows Piers
  get a Control tap from the Deck). Linux and Mac hosts keep their mapping. Each saved connection
  can choose Automatic, Windows App style, ⌘ and Control swapped, or ⌘ is
  Control.
- **AltGr and the ISO key.** On a Windows host right ⌥ is AltGr, so a
  Norwegian (or any European) Windows layout types `@`, `{`, `€` where it puts
  them; **Left Option is also AltGr** does the same for left ⌥. The ISO key
  next to left ⇧ (`<>`) is its own key on Windows and Linux hosts instead of
  being folded into another, and the Deck corrects macOS's ISO keyboard swap
  of that key and the key above Tab. Both are new key identifiers sent only
  to hosts that announce them (`input_capabilities.extended_keys`).
- **Test connection, and `arcen-deck diagnose`.** The Deck's connection
  editor and connection details gain **Test connection**, and the Deck binary
  gains `diagnose <host[:port]>`. Both check, step by step, name resolution,
  UDP 18444 with a real QUIC handshake (telling a closed port from a silent
  one where the network answers), the certificate against the identity the
  Deck remembers (or a `--pin-sha256`/`--pin-spki` you supply), the Pier's
  greeting before sign-in, the path MTU and the round trip against the Pier's
  own QoS thresholds — and say what to do about each failure, with a stable
  `diag.*` code and **Copy report**. No user name or password is asked for or
  sent; a Pier discloses its version only after sign-in, so the test stops
  there. The steps, findings and wording are shared code
  (`arcen_session::connection_diagnostics`) for every future Deck.
- **Colour check.** Session › Colour Check… (and Settings › Streaming ›
  Colour check…) is a checklist for colourists with a ✓/⚠/✗ verdict and a
  stable `colour.*` code per item: what the host actually serves (preset,
  codec profile, chroma, depth, transfer, primaries, matrix, range, the
  decoded frames' headers, the host's reason for any downgrade, the implied
  capture source, the encoder) against what was asked for; what this Mac does
  (reference mode, colour profile, HDR headroom, ten-bit Metal layer, EDR read
  back from the layer, 1:1 scaling); and an on-demand exactness measurement:
  **Save pattern…** writes the chosen test pattern at the stream's exact size,
  and **Measure** compares every decoded pixel with it straight from the
  decoder's planes, in RGB and in the stream's own ten-bit codes, and says
  when a host has re-encoded the desktop's sRGB as BT.709 (what a Windows
  Pier capturing linear FP16 serves for Grading) or when an overlay covers
  part of the pattern. One click
  switches to Grading or HDR and reconnects, enters fullscreen, or turns on
  Retina-resolution streaming; host prerequisites (Linux depth 30, Mutter 50,
  Windows HDR) are explained and linked, never changed. **Copy report** and
  **Save report** give a timestamped text report. The items, rules and
  wording are shared code (`arcen_media::colour_check`), as is the pattern
  measurement (`arcen_media::test_pattern::measure_biplanar`).
- **Operations reference.** [Error codes](docs/operations/error-codes.md)
  lists every code Arcen reports — connection-test findings, resume refusals,
  Linux sign-in refusals, polkit, deskside, native-seat, Wayland provider,
  qualification and installer reasons, lane refusals and the warning and
  error log events — with what to do.
  The [configuration reference](docs/operations/config-reference.md) lists
  every `pier.json` key for Linux, Windows and macOS with type, default,
  meaning and the release that introduced it, and
  [Troubleshooting](docs/operations/troubleshooting.md) is organised by
  symptom. `scripts/check_shared_contracts.py` now fails when a key or code
  is added without its row. The README opens with where to start for an
  artist, an administrator and a security reviewer.
- **Real cursor images on X11 and Windows hosts.** With the Mac's own cursor
  (local cursor), the Deck now shows the host application's actual cursor —
  brush circles, crosshairs, colour pickers, custom CSS cursors — instead of
  the nearest named shape, on Linux X11 (XFixes) and Windows Piers as it
  already did on Wayland desktops with cursor metadata. Each image crosses
  the link once and is drawn at the size it has in the video, so a brush
  outline matches the brush on the canvas at any zoom, and at the display's
  full resolution on a Retina Mac (thin crosshairs and brush outlines stay
  crisp instead of being upscaled from a one-pixel-per-point bitmap). Monochrome Windows
  cursors that invert the screen get a white outline so they stay visible.
  An application that hides the pointer hides it on the Deck too. Older
  Decks keep the named shapes. The macOS Pier still sends named shapes only.
- Deck: Session → Connection Health shows a `Path:` row with the Pier's own
  measurement of the QUIC path that carries the video — congestion
  controller, smoothed and minimum RTT, loss (packets lost of packets sent),
  wire rate and window, graded good/fair/poor — instead of a guessed path
  type. Linux and Windows Piers send it in `health_stats` (`path`, additive);
  an older Pier is shown as not reporting it.
- `ARCEN_QUIC_CONGESTION` also accepts `newreno` and `bbr-guarded`.
- Deck CLI: `media-smoke` writes per-frame delivery records to the file named
  by `ARCEN_MEDIA_SMOKE_STATS` for path measurements.

### Changed

- **Wayland hosts type committed text by proven key strokes.** Paste as
  Keystrokes, Characters mode and input-method commits into a GNOME Wayland
  session are typed by keycode, with Shift and AltGr held by their own keys,
  only where the session's keymap proves the character — every key is
  replayed on the compositor's keymap first. Before, they went as keysyms
  through Mutter, which presses only the first key listing a keysym and
  drops its higher-level modifiers: on UK and US layouts `<` and `>` came out
  as `z` and `x`, and a session with two groups of the same name (GNOME adds
  the locale's layout) typed everything unchecked. Characters the layout
  lacks (on a UK layout: `æøå`, `é`, `ü`, emoji, Cyrillic) are refused and
  counted to the Deck instead of being typed wrong, until GNOME offers an
  input-method route (`ei_text`). When the session's keymap holds several
  different layouts (several input sources, or a Norwegian source on an
  English-locale host, where GNOME adds the locale's layout), only
  characters every layout types
  with the same keys are typed by key, since GNOME does not reliably say
  which layout is active; the rest are refused and counted.
- **The Windows Pier injects keys by scan code.** Keys are typed by their
  position (`KEYEVENTF_SCANCODE`), so the Windows session's own keyboard
  layout decides the character, exactly as for a local keyboard; punctuation
  on non-US layouts (`+`, `ø`, `æ`, `å`, `-`) was wrong before. Keypad, lock,
  Pause and Print keys keep their virtual keys.
- The administration guide's key tables and troubleshooting table moved into
  the configuration reference and the troubleshooting page; the guide links
  to them and keeps the policy explanations.
- BBR stays the QUIC congestion controller. Measured on a Wi-Fi + VPN link,
  BBR keeps 16 Mbit/s and ~19 ms frame age under 1 % random loss where Cubic
  falls to 6 Mbit/s and ~675 ms. `ARCEN_QUIC_CONGESTION=bbr-guarded` selects
  an experimental loss-bounded variant for shallow-buffered slow
  bottlenecks (its first version cut plain BBR's 54 % loss at 8 Mbit/s to
  11 %); it is not the default yet because review found a case where it does
  not engage.
- Linux Pier: encoded video no longer piles up on the host when the network
  is slower than the encoder. Once two frames wait for the network and the
  oldest has waited two frame periods, the Pier stops taking frames from the
  encoder, so the next picture captured is the current screen; no encoded
  frame is dropped for it; at an 8 Mbit/s bottleneck frame age fell from
  ~1.09 s to ~0.76 s and the frame rate rose from ~23 to ~26 fps. The frame timestamp is now taken when the Pier
  reads the frame from the encoder, so the Deck's frame age includes every
  wait on the host. `ARCEN_VIDEO_BACKLOG=legacy` restores the previous queue.

### Fixed

- Linux Wayland Pier: a host idle for ten days no longer loses X11
  applications — or, on GNOME, every other user's desktop — to whichever user
  signs in first. systemd's daily `/tmp` clean removed `/tmp/.X11-unix` (it
  is created only at boot) and the next desktop's compositor recreated it as
  its own user. The installer now keeps `/tmp/.X11-unix` and `/tmp/.ICE-unix`
  out of that clean (`/etc/tmpfiles.d/arcen-pier.conf`), and the Pier checks
  the directories before every desktop with X11 applications: a missing one
  is recreated root-owned `1777`; one owned by a user is logged as an error
  with its fix (`x11_unix_dir_foreign_owner`) and that desktop starts without
  X11 applications instead of aborting. A qualification on such a host
  records the fault as a host precondition, re-checked at every start,
  rather than as failed X11 support. The new `arcen-pier repair-x11-unix-dir`
  fixes both directories with the system's own tmpfiles entry, and
  `deploy-pier.sh --uninstall` removes what that script installed.
- Deck CLI `media-smoke`: a run that times out now always prints what arrived
  and what is missing, and names a silent host audio source when video arrived
  but no audio packet did. It used to stop with a bare "timed out waiting for
  media", which read as a stalled stream on an idle Windows desktop (a Windows
  Pier sends no audio while nothing plays).
- **Windows Grading keeps the desktop's colours code for code.** The Windows
  Pier captured Grading as linear light and re-encoded it with the BT.709
  curve, while the Deck shows SDR as sRGB: every colour moved, by up to 16/255
  in the shadows, and dark tones looked crushed. It now re-encodes with the
  sRGB curve Windows linearised with, so the stream carries the desktop's own
  values (every eight-bit code comes back unchanged), and it says so: the
  stream and the session report the sRGB transfer (`srgb`, H.273 13) and the
  session is still Grading. Needs a 0.18.0 Deck, which announces that it
  presents sRGB streams (`srgb_transfer`); older Decks keep the old encoding.
  HDR, Auto and Speed are unchanged.
- **HDR at Retina size no longer kills the Windows session.** A Deck asking
  for HDR at 3600×2260 (a 14-inch MacBook Pro streaming at Retina size) got
  "connection timed out" and the Pier's crash watchdog restored the display,
  on 0.17.0 too. The HDR10 display EDID declares a 550 MHz HDMI link, and a
  10-bit 3600×2260@60 mode needs 655.6 MHz, so Windows switched HDR on but
  never activated it; the session agent waited for HDR past the broker's 30 s
  start deadline and was terminated. The shared rule
  (`arcen_outputs::edid::hdr10_refresh_hz`) now creates an HDR display at the
  highest refresh whose 10-bit link fits, stepping down through multiples of
  the HDR stream rate (3600×2260 runs at 30 Hz, the HDR stream's own 30 fps,
  so the stream is unchanged); `generate_hdr10` refuses a mode its link cannot
  carry; and a multi-monitor HDR request at such a mode is refused at once
  with a reason instead of timing out. Grading and Speed keep their refresh.
  Any other HDR that Windows will not activate now ends within the shared
  setup budget (`arcen_session::setup_budget`: 25 s, 5 s inside the broker's
  30 s wait) with the display restored and a coded refusal the Deck shows
  (`hdr_not_activated`, `hdr_link_exceeded`; see Error codes), instead of the
  agent being terminated.

- **Retina fullscreen on a notched Mac is 1:1 again.** The Deck sized the
  stream from the notch (`safeAreaInsets.top`, 38 pt on a 14-inch MacBook
  Pro), but a fullscreen window starts below the menu bar, one point lower:
  the host served 3600×2262 pixels for a 3600×2260 viewport and the Deck
  shrank every frame by 0.1 %. The top inset now comes from the Deck's own
  fullscreen window on that display (otherwise the larger of the notch and
  the menu bar height), which does not change when the menu bar hides, so
  the stream matches the viewport exactly (found by the colour check's Scale
  item).
- **Grading on a Linux Xorg host from a Retina Mac.** The ten-bit (depth-30)
  capture path accepted at most a 3840 by 2160 box, so a Deck asking for its
  Retina size (for example 3600×2262: taller than 2160 but fewer pixels than
  3840×2160) failed the GPU encoder's check, fell through to the software encoder and was refused with "Grading
  requires a hardware 10-bit 4:4:4 encoder" although the GPU was idle and
  licensed. The limit is now what it always meant, a pixel budget of one
  3840×2160 frame (edges up to 8192): 3600×2262 is served as asked, and a
  larger display (5K, Pro Display XDR) is served at the largest size with its
  shape that fits, on connect and on a live resize, instead of being refused.
- **A refused Grading or HDR session says why the GPU was not used.** When
  the hardware encoder is unavailable and software cannot serve a fidelity
  request, the Pier's refusal now names the hardware cause (the desktop's
  size or depth, every encoder session in use, no usable device, driver or
  vGPU licence, a missing runtime, an unsupported 10-bit format) instead of
  only saying that software is 8-bit; the Deck's Colour check shows it.

## [0.17.0] — 2026-10-21

The Linux Pier now serves Wayland as well as X11. On a host without an Xorg
server each sign-in gets its own GNOME Shell/Mutter desktop, proven on that
host before anything is served; on stock GNOME a Wayland desktop can also run
natively on its own logind seat, which gives the Deck's pen real pressure and
tilt there. This release also closes a polkit hole in every earlier Linux
release: a remote X11 desktop received the console's administrative rights
(see Security).

Deck and Pier must be updated together: 0.17.0 speaks protocol v5 on
isolated QUIC lanes, and a 0.16 Deck or Pier is refused with a message saying
to update both to the same release. Linux operators: read the upgrade notes
first.

### Upgrade notes

- **Linux hosts become muted by default.** The Linux Pier now honours
  `audio.local_playback`, and its default, `muted`, silences the host's own
  speakers for the whole session. On X11, an Arcen sink becomes the default
  of the user's audio server, every other sink is muted and every stream is
  moved onto it; on Wayland, the session's private audio graph has no
  speaker in it. A host that cannot prove the silence refuses the session and
  tells the Deck why. Set `audio.local_playback` to `audible` to keep the
  speakers playing. If the same user is also signed in at an X11 host's
  console, that console is silent during the remote session too.
- **X11 desktop depth is `auto` (new key `video.x11_desktop_depth`).** An
  Xorg desktop is now created at depth 30 only for a Grading or HDR session
  (or a declared `rec2100-pq` desktop), and at depth 24 otherwise, because
  at depth 30 Firefox and Chrome render without the GPU. The depth is fixed
  for the desktop's life: Grading or HDR on a desktop created for Auto or
  Speed is served visibly as Auto (`eight_bit_source`), and the Deck says
  how to get a ten-bit desktop — sign out and reconnect with Grading, or have
  the host pinned. Grading hosts (Flame, Resolve) should set
  `video.x11_desktop_depth` to `30`, which restores the earlier behaviour.
- **One X11 desktop per host.** An X11 desktop makes its own session the
  active one on the console seat. Sign every user out of the old build's
  desktops before upgrading: a desktop still held by the previous Pier, or by
  a second Pier on the same host, stops the next one from starting
  (`session launcher protocol failed`).
- **Wayland hosts re-qualify themselves.** The qualification record moves to
  schema 3, and an update that makes the record stale (Pier, GNOME/Mutter,
  PipeWire or GPU driver) is now re-qualified by the service at start instead
  of refusing (`capture.wayland_auto_requalify`, default `true`; bounded; a
  failure on the same stack is not retried for 6 hours). Expect one
  re-qualification after this update. `false` refuses and names the command.
  The Pier handles SIGHUP (logrotate) and SIGTERM from the start of
  start-up: a reload during the re-qualification only reopens the log, a stop
  abandons it cleanly, and an interrupted attempt is retried rather than
  counted as a failure. The unit also restarts the Pier after a death by
  SIGHUP or SIGPIPE (`RestartForceExitStatus`).
- **A refused desktop takeover reads as one.** Signing in while another
  user's desktop waits for its owner to reconnect now says so instead of
  "authentication failed: active session requires resume authentication".
- **The GNOME prefix is optional.** `capture.wayland_compositor_prefix`
  (default: the distribution's `/usr`) lets Arcen's Wayland sessions run a
  GNOME Shell and Mutter installed beside the system's, such as
  `/opt/arcen-gnome/51`, without changing the system GNOME. It is needed only
  for Grading and HDR on Wayland where the distribution ships GNOME 49 or
  older. Arcen does not ship a prefix; the Pier and the installer verify the
  whole prefix (root-owned, not writable by group or others, no link out of
  it) and refuse one that fails. See `docs/operations/pier-administration.md`.
- **Linux hosts get a polkit rule.** The installer writes
  `/etc/polkit-1/rules.d/05-arcen-remote-session.rules`, and the Pier
  rewrites it at every start (see Security). For a remote seated desktop it
  answers before later rules, so a site rule numbered after `05-` (or a
  `.pkla` file) that denies such users an action, or demands an
  administrator for it, no longer reaches them: renumber site restrictions
  and deliberate grants for remote desktop users `00`–`04`.
  `arcen-pier polkit-rule status` lists the later rules that may be
  affected. On polkit older than 0.106 (no JavaScript rules, for example
  Ubuntu 22.04) the rule cannot be installed: X11 desktops are still served,
  with a `SECURITY:` error in the log at start and at every sign-in.
- **Native Wayland desktops are on where proven.** `wayland.session_mode`
  defaults to `auto`: when the host's qualification (at install, or the
  automatic re-qualification) proves a native desktop, a Wayland desktop
  runs natively on its own seat, one at a time, and gets a real pen. That needs an NVIDIA GPU without display connectors
  (a vGPU or data-centre GPU) and the in-tree `vkms` module; anything else
  stays headless, as before. Set `headless` to keep every desktop headless.
  A display-manager restart while a native desktop runs freezes it; the
  installer explains this and offers `--disable-gdm`.
- **The lab seat pen stays off.** `wayland.lab_seat_pen` (default `false`)
  works only with a compositor built with the unmerged mutter!5224 and an
  Arcen lab edit, and applies only to headless desktops. Do not enable it on
  a production host; native desktops give a real pen on stock GNOME.
- The Linux release Pier links `libpipewire-0.3.so.0` on every host, X11
  included, alongside `libpulse` and `libpam`; the installer names a missing
  library before writing anything. It is still built on Rocky Linux 9
  (glibc 2.34).

### Security

- Linux Pier (X11): a remote desktop no longer gets the console's polkit
  rights. polkit counts a session with a seat as local and ignores that it
  is remote, and every X11 desktop is a session on `seat0`, so in every
  earlier Linux release (0.16.1 and older) its user could power off or
  reboot the host, control its networking, mount disks or set the clock
  without a password. The installer now installs
  `/etc/polkit-1/rules.d/05-arcen-remote-session.rules` (rewritten by the
  Pier at every start, removed by `--uninstall`), which gives sessions that
  logind records as remote, that an Arcen launcher opened, or that sit on an
  Arcen desktop seat each action's remote (`allow_any`) answer. Before
  handing a desktop over, the Pier asks polkit (`pkcheck`) about power off,
  reboot, suspend, network control, mounting and the clock, and refuses the
  desktop, killing its processes, on any grant; a seated desktop is also
  refused when polkit does not answer, or before launch when the rule is
  missing. Native Wayland desktops use the same rule and proof; headless
  ones are seatless and were never affected. Administrators: site
  restrictions for remote users must be numbered `00`–`04` (see Upgrade
  notes); on polkit older than 0.106 the rule cannot be installed, and X11
  desktops are served with a `SECURITY:` error logged at start and at every
  sign-in — upgrade polkit or restrict who may sign in. Reported internally;
  no CVE.
- Linux Pier: the sign-in password is wiped on every refused sign-in, not
  only on success; the session launcher reads it into a single wiped buffer
  and is no longer dumpable, and the Pier wipes its copies of the sign-in
  request.
- Linux Pier (X11): the console's keyboard and mouse no longer drive a remote
  desktop. Every earlier Linux release let the dedicated Xorg add every
  input device on the console seat, so a person at the physical keyboard, or
  anyone with the virtual machine's console, could type and click in a
  remote user's desktop; a second X11 desktop on the same host also received
  the first one's virtual input. Each desktop's virtual devices are now named
  for its display, and its X server ignores every device without Arcen's
  vendor id and that name. This stops ordinary console devices; it is not
  proof of origin (see `docs/security/trust-boundaries.md`). Reported
  internally; no CVE.
- Linux Pier: PrintScreen is sent as `KEY_PRINT` instead of `KEY_SYSRQ`, so
  a remote Alt+PrintScreen+key can no longer run a kernel SysRq action.
  Applications still receive `Print`. No Deck key maps to a power, sleep or
  wake key, and a test keeps it so.
- Linux Pier (Wayland): Native tablet (USB bridged) and the quarantined
  raw-HID passthrough are refused. A headless compositor opens no input
  devices, so a bridged tablet would have reached the machine's physical
  seat instead of the Deck's desktop. The Deck is told why and pointed to
  Tablet support.
- Linux Pier (Wayland): the sign-in password that unlocks the GNOME keyring
  is read by a non-dumpable session agent into one zeroizing buffer, never
  sits in a stdin buffer, and appears in no process's argv or environment.
- Linux Pier: clipboard helpers receive the negotiated directions, so the
  host never reads a content kind the Deck did not ask for, and a refused or
  abandoned X11 INCR copy can no longer leak a stale chunk into the next
  copy.
- Linux Pier (Wayland): the Xwayland stale-socket sweep removes a display's
  socket only while its lock is the user's own file naming the session
  compositor, so it cannot remove another live desktop's socket.

### Added

- **Linux Pier: headless Wayland desktops** on hosts without an Xorg server.
  Each sign-in gets its own seatless logind session running a headless GNOME
  Shell/Mutter (40 or later) with a private D-Bus bus, PipeWire and
  WirePlumber; video goes through Mutter's ScreenCast into the fused capenc
  and input through the session's RemoteDesktop injector. The Pier chooses
  the provider at start from host facts (`capture.provider = auto`): an
  installed Xorg server keeps X11, and an existing installation never moves;
  without Xorg, Wayland needs a DRM render node and a qualification record
  (`arcen-pier wayland qualify --write`). `xorg` or `wayland` overrides the
  choice. Within a Wayland session:
  - **Speed** where the record proves the rate: GPU import (DMA-BUF → EGL →
    CUDA, same GPU, explicit sync, verified against a host copy) measured
    over the previous record's displays, displays Decks asked Speed for and a
    ladder of 1920×1080, 2560×1600 and 3840×2160, HEVC and H.264, without
    operator sizes. A smaller display is admitted on a larger measured one
    (`inferred`). A Speed stream that encodes below 57 fps while the desktop
    delivers 60 is re-served as Auto (`speed_runtime_below_floor`).
  - **Grading and HDR on GNOME/Mutter 50 or later**, as compositions of
    their own (the session's compositor alone runs with
    `MUTTER_DEBUG_FORCE_HDR`), each admitted only when a per-host precision
    proof passes: HEVC 4:4:4 10-bit, BT.2020/PQ for HDR and BT.709 for
    Grading, at the 30 fps contract. GNOME 49 and older serve both visibly as
    Auto (`compositor_lacks_hdr_screencast`).
  - **One to four monitors** (Match My Layout) as Mutter virtual outputs in
    its physical or logical layout mode, moved in place when a later sign-in
    brings another layout, and **Remote UI scale** applied as the output's
    GNOME scale. A Windowed Deck's single-output resize is followed.
  - **The Deck draws the desktop's own cursor** as its native macOS cursor
    where qualification proves Mutter's cursor metadata (`CursorUpdate` on
    the input/cursor lane, each image sent once, a hidden pointer included).
    Older Decks and a "cursor in remote video" preference keep the cursor in
    the video.
  - **Clipboard** both ways, text and PNG, under the same `clipboard.*`
    policy as Xorg, through Mutter's RemoteDesktop clipboard and the shared
    `arcen_media::clipboard::bridge`.
  - **Audio**: a private audio graph per session, so applications that use
    PipeWire natively and PulseAudio applications are both heard by the Deck
    (and not in the room), and the Deck microphone is visible to both.
  - **Typed text**: the Mac's input method (dead keys, IME, emoji) and
    keystrokes the host's layout would type differently arrive as
    `text_commit`. The session types only what its compositor's keymap can
    type; other characters go through the compositor's `ei_text` when
    offered and are otherwise refused, shown on the Deck. Deck: Settings →
    Input → Typing (Auto, Keys only, Always characters; which Option key is
    Alt).
  - **X11 applications** through the compositor's Xwayland where
    qualification proves the X11 pointer (`platform.session.wayland_x11_apps`,
    `auto`, `on` or `off`).
  - **A GDM-like login**: the session manager, the keyring unlocked with the
    sign-in password, the settings daemons, the user's autostart programs and
    the accessibility bus run; idle lock and Log Out work; GNOME's first-login
    assistant runs unless `wayland.first_login_setup` is `skip`.
  - **Software encode** on a host without NVENC: OpenH264, H.264 4:2:0, up to
    30 fps, outputs fitted to 1920×1200.
  - A capture that ends on its own is restarted inside the attachment,
    keyframe first (three restarts a minute, then the Deck is told); a
    desktop whose compositor died ends the attachment, and the next sign-in
    starts a new one.
  - With `video.qp_map` on, the compositor's own damage regions bias the QP
    map.
  - **Pen discovery**: each session asks its compositor for an EIS stylus
    (`ConnectToEIS` and a libei handshake through the pure-Rust `reis`
    client) and advertises pen axis by axis only when one is bound. No
    released GNOME offers one (`stylus_not_offered`), so a headless desktop's
    pen acts as a mouse; native desktops (below) give a real pen.
    `arcen-pier wayland eis-probe --socket PATH` probes any libei socket.
  - **Lab only, default off:** `wayland.lab_seat_pen` gives each headless
    desktop its own virtual tablet on a private udev seat, for a compositor
    built with the unmerged mutter!5224 plus an Arcen edit;
    `capture.wayland_compositor_prefix` and
    `packaging/linux/tools/gnome-prefix/` build and run such a GNOME beside
    the system's.
  - `arcen-pier wayland qualify --record PATH [--temporary-user NAME]
    [--allow-running-service]` and `capture.wayland_qualification_record`
    for development Piers beside an installed one.
- **Linux Pier: native Wayland desktops with a real pen on stock GNOME**
  (`wayland.session_mode`: `auto`, the default, `native_seat` or
  `headless`; ADR 0015). Headless Mutter opens no input devices, so a
  headless desktop cannot have a tablet. A native desktop runs the same
  stock compositor natively (`--display-server`) on its own logind seat
  (`seat-arcen-pen-<id>`) holding the NVIDIA card node, the in-tree `vkms`
  card as the display head Mutter requires, and the desktop's virtual
  tablet; logind hands the compositor those devices and revokes them at the
  end. The Deck's pen then reaches Firefox, Chrome and Xwayland
  applications as a pen with pressure and tilt (`zwp_tablet_v2`). Speed,
  audio, clipboard, cursor and outputs are those of any Wayland desktop.
  - Qualification proves it (record section `native_seat`, schema still 3):
    a Wayland client of the qualification desktop's compositor must see the
    desktop's own tablet and its pen with pressure and tilt, and polkit
    must admit the session on its seat (`polkit_effective`). A host that
    cannot have one records why (`gpu_has_connectors`, `vkms_missing`, …).
  - `auto` makes a desktop native only with that proof and a seat that can
    be built now; otherwise headless, and the Pier logs `this Wayland
    desktop is headless (no real pen)` with the reason. `native_seat`
    refuses instead (`native_seat_unavailable`). One native desktop per
    host.
  - Every host change is volatile — a rule in `/run/udev/rules.d/`, a `/run`
    override of `61-mutter.rules` without its `vkms` ignore lines,
    `modprobe vkms` — and is undone at the desktop's end, at Pier stop and
    by the sweep at Pier start. No ACL is granted on an input device.
  - A native desktop's session has a seat, so it is protected by the same
    polkit rule and proof as an X11 desktop (see Security): without the rule
    or with polkit older than 0.106 the seat is never built, and a failed
    proof refuses the desktop; until the Pier restarts its later desktops
    are then headless (`auto`) or refused (`native_seat`).
- Linux Pier: `arcen-pier polkit-rule install|status|print|remove|check
  <pid>`. `status` also lists later site rules and `.pkla` files that
  restrict active sessions and runs a console self-test; `check` shows the
  admission a desktop holding that process would get now.
- Linux installer: `--disable-gdm` and `--restore-gdm`. A display-manager
  restart while a native desktop runs would put a login screen on its seat;
  the installer explains this and changes nothing without the option.
  `--disable-gdm` records the display manager's state, and `--restore-gdm`
  and `--uninstall` put it back.
- Linux installer: chooses the provider from the host's facts and explains
  it, qualifies Wayland during the install and starts the service only if
  that passes, and refuses a host with neither Xorg nor DRM before writing
  anything, naming the GPU driver to install. New `--check` (detection
  only) and `--qualify-output`; detection and qualification output go to
  `/var/log/arcen/install.log`. The embedded Pier is test-run before
  writing. Uninstall also removes the qualification record and a leftover
  temporary qualification account, and closes UDP 18444 when the installer
  opened it.
- Every Linux Pier: a fresh sign-in by the same user takes over that user's
  detached session instead of being told to resume (the old Deck's resume is
  then refused with the reason); an attached session is checked for liveness
  first; another user is still refused.
- Every Linux Pier: desktops get the user's locale (AccountsService, then
  `/etc/locale.conf`) instead of the C locale, the user's login-shell
  environment, and a logind sleep inhibitor for as long as they are served,
  so the host never suspends under a remote desktop. The support bundle
  records the provider selection and the Wayland record.
- Windows Pier: `platform.font_smoothing = "grayscale"` (default
  `unchanged`) switches ClearType to grey-scale smoothing for the length of a
  session, so text reaches the Deck without colour fringes, and restores it
  afterwards, also after a killed agent. The user's stored preference is
  never written.
- macOS Deck: pen samples are sent as AppKit delivers them (the device's
  rate) behind ordering barriers for keys, buttons and scrolls; every pointer
  move and button of a UI pass is sent in order; the served-pipeline badge
  shows the host's reason and remedy (`ServedPipelineMsg.detail`); on-glass
  latency telemetry; `media-smoke` clipboard drivers
  (`ARCEN_MEDIA_SMOKE_CLIPBOARD_*`).
- Shared: `arcen_input::pen_emulation`, `arcen_input::pen_forwarding`,
  `arcen_keel::ArrivalPacer`, `arcen_session::compositor_prefix`,
  `arcen_session::native_seat`, `arcen_session::polkit_remote`, and the
  additive protocol fields listed in `shared/protocol/WIRE.md`.
- Contributors: each shared crate carries a consumer register
  (`shared/*/consumers.json`), and `scripts/check_shared_contracts.py
  --impact` lists the validation a shared change needs; see
  `docs/architecture/shared-consumers.md`. `scripts/lab/realscreen/`
  measures latency on the real Deck window with real input.

### Changed

- Every Pier and the macOS Deck run each session on isolated QUIC lanes —
  control, video, audio output, input/cursor, microphone and bulk on their
  own streams of one connection (ADR 0014, protocol v5). A blocked video
  stream no longer holds audio, input or control.
- Linux Pier (Xorg): Speed reaches 60 fps. NvFBC frames are sent as they
  arrive, paced to the contract (shared `ArrivalPacer`), and a new desktop's
  head runs at the paced refresh (120 Hz for Speed); the lab measured 33–43
  fps before.
- Linux Pier (Xorg): Grading reads, converts and uploads only what changed
  on the depth-30 desktop, with page-locked staging and persistent
  conversion threads. A single-head screen's `Virtual` is raised for a
  served display larger than 3840×2160.
- Every Pier: rate control judges loss only over at least 200 packets
  (quiet streams are pooled), never makes a severe cut from a keyframe's own
  interval, and logs why each second's frame rate was what it was. Auto and
  Speed may climb past their shape budget, up to four times it and 250 Mbps,
  on a path that has proved it.
- macOS Deck: 4:2:0 streams reconstruct chroma guided by luma, so text edges
  are no longer coloured in hard steps; layer scaling is pixel-exact. 4:4:4
  is unchanged.

### Fixed

- Windows Pier: a session that starts after the console display has gone to
  sleep now streams at full rate from the start; before, it ran at 4–8 fps
  until the Deck sent input.
- macOS Deck: menu commands, including Quit, are applied at once while the
  Deck is idle.
- Linux Pier (X11), Tablet support: each pen sample now reaches the desktop
  once, through the virtual tablet only. Before, every sample was also sent
  as a mouse event, so Firefox switched between mouse and pen mid-stroke and
  Chrome lost pressure on part of a stroke. Contact follows the Deck's tip,
  so taps and very light strokes ink; tilt arrives at the right angle;
  barrel buttons follow the standard Linux mapping (lower = middle click,
  upper = right click).
- Linux Pier: signing in while another remote desktop holds the host's X11
  seat is refused with "Another remote desktop is active on this host (one
  X11 desktop per host)" instead of "session launcher protocol failed".
- macOS Deck: the log names the exact build (version, build id, source
  revision) at start and on every connection; the Linux Pier logs the
  connecting Deck's build.
- Every Pier and the macOS Deck: clipboard items of 1 MiB or more reach the
  other side again, both ways. Each full chunk was 25 bytes over the bulk
  lane's 1 MiB bound and was refused before it was sent.
- Every Pier and the macOS Deck: a session's last words survive its close.
  The Deck shows why a Pier refused it instead of `control lane ended:
  ConnectionLost`, and a Deck that disconnects is logged as `client_closed`
  and releases the desktop (ADR 0014 amendment 7). A 0.16 Deck reaching a
  0.17 Pier is told to update.
- macOS Deck: asking for a client-drawn cursor always keeps this Mac's
  cursor (a host that drew its cursor into the video made the Deck hide its
  own, so the pointer moved at the video's rate: 77–96 ms behind the mouse,
  now 13–15 ms). With Tablet support, the pen keeps this Mac's cursor on a
  desktop that hides its pointer for the pen.
- Linux Pier (Wayland): a Deck that draws the cursor never also gets one in
  the video (Mutter's `hidden` cursor mode when metadata is unavailable).
- Linux Pier: a pen held still and then moved is seen at once (libinput's
  50 ms forced proximity-out is disarmed per virtual tablet), and on Xorg
  the pen is as quick as the mouse (a private libinput quirk turns
  smoothing off for the Arcen pen only, via `LIBINPUT_QUIRKS_DIR`).
- Linux Pier (Wayland): the absolute pointer lands where it should at a UI
  scale above 1 in the logical layout, and during Grading and HDR.
- Linux Pier (Wayland): the accessibility bus works under SELinux (it failed
  for every application on Rocky Linux 10).
- Linux Pier: clipboard — an oversize or odd host copy is refused
  (`clipboard_host_item_refused`) instead of ending the attachment; a
  transfer the bulk lane cannot take fails visibly
  (`clipboard_transfer_failed`); an item streaming each way at once no
  longer deadlocks; a copy while all 32 X11 properties are draining is
  refused as `busy`.
- Linux Pier: the X11 cursor watcher's first shape is no longer lost; an
  Xorg desktop survives a refresh or Remote UI scale change; an operator's
  QP map no longer follows a stream into OpenH264, where it failed the
  attachment.
- Linux Pier on SELinux-enforcing hosts (RHEL, Rocky, Alma, Fedora):
  sessions no longer fail with `pam_systemd did not create the required
  logind session`. The installer loads a one-rule SELinux module (`arcen`)
  and unloads it on uninstall; users mapped to confined SELinux users are
  refused with a clear error. See `docs/security/linux-selinux.md`.
- Linux Pier: `SESSION_END`, `SESSION_INTERRUPTED` and `HEALTH_SNAPSHOT`
  are no longer dropped once the Deck has reported its network.
- macOS Deck: a health ping queued before sign-in no longer declines the
  sign-in.
- Linux installer: `--uninstall` on an uninstalled host is quiet, and
  `--purge` also removes `/run/arcen`. Windows installer: `--dry-run` on a
  clean machine no longer fails reading a `host.crt` it did not write.
- Stream logs report the visible picture size, not NVENC's padded coded
  size.
- macOS Pier package: `Arcen Pier.app` now carries the AGPL licence
  (`LICENSE`) and `THIRD_PARTY_NOTICES.md` in `Contents/Resources`, and
  `build-pier-pkg.sh` refuses a payload whose copies are missing or differ
  from the source.
- macOS Deck: `Arcen Deck.app` and `Arcen Deck Intel.app` now carry the AGPL
  licence (`LICENSE`) beside `THIRD_PARTY_NOTICES.md` in
  `Contents/Resources`.
- macOS packaging: the Pier package and the Deck release zip no longer carry
  an AppleDouble (`._`) companion for every file. On macOS 26 `pkgbuild` and
  `ditto` encoded the irremovable `com.apple.provenance` attribute as one;
  the package build strips them and refuses a package that still has any.

### Known limitations

- Linux Pier (Wayland): pen. A real pen needs a native desktop: one per
  host, on an NVIDIA GPU without display connectors with `vkms` and polkit
  0.106 or later, verified end to end only on Rocky Linux 10.2 / GNOME 49 /
  NVIDIA GRID vGPU (other hosts are admitted or refused by the same
  qualification). On a headless desktop the Deck's pen acts as a mouse
  (GNOME 49: `stylus_not_offered`; 40: no EIS at all) until GNOME's EIS
  offers a stylus. Native tablet (USB bridged) is refused on every Wayland
  desktop. A display-manager restart freezes a running native desktop
  unless the display manager is disabled.
- Linux Pier (Wayland): Grading and HDR need GNOME/Mutter 50 or later (on
  Rocky Linux 10, which ships 49, that means the optional prefix) and one
  output; Speed, Grading and HDR on several outputs are served as Auto.
  Speed is admitted only on a qualified host and display size. A Wayland
  desktop opened for Auto or Speed is not on the wide route: reattaching to
  it with Grading or HDR fails visibly; sign out and reconnect.
- Linux Pier (Wayland): no hardware encode on Intel or AMD (VA-API); such a
  host uses OpenH264. Characters outside the session's keyboard layout are
  refused until GNOME ships `ei_text`. Measured in the lab, a Wayland
  desktop adds about 10–20 ms between host paint and glass compared with
  X11 at the same frame rate.
- Linux Pier (Xorg): a Grading desktop runs at depth 30, where Firefox and
  Chrome render in software (about 8 fps in the lab). Arcen's virtual
  devices are still console-seat devices: a login screen or local session
  that becomes active on the console while a remote session is attached can
  receive the remote user's input.
- Typed text and native cursor images are Wayland-only for now; the Windows
  Pier does not implement `audio.local_playback`.
- macOS Pier: unchanged preview status (cold-boot login-window operation,
  multi-display on physical hardware and end-to-end microphone capture are
  not qualified).

## [0.16.1] — 2026-10-17

### Fixed

- Linux Pier: sessions on a freshly installed host no longer fail with
  `dedicated Xorg failed to start`. The packaged `/etc/arcen/xorg.conf` named
  a fixed legacy GPU `BusID`, so on any other machine the NVIDIA driver
  reported `No devices detected`; legacy fixed BusIDs are now replaced with
  the host's detected NVIDIA BusID. The template now ships without
  a `BusID`; the installer finds the NVIDIA GPU in sysfs and sets it (keeping
  a correct one, backing up an operator file to
  `xorg.conf.before-<version>`, warning when there is no NVIDIA GPU), and the
  Pier re-checks it in memory before every X server, so an existing stale file
  keeps working until the installer is rerun. The decision is the shared
  `arcen_outputs::xorg_gpu`.
- Linux Pier: capture opens CUDA on the dedicated X server's GPU, by PCI
  address (`ARCEN_CAPTURE_GPU_PCI`, logged as `capture_gpu_pci=`), instead of
  CUDA device 0, so a host with different GPUs cannot capture or encode on one
  the screen does not live on. Capture children use
  `CUDA_DEVICE_ORDER=PCI_BUS_ID`.
- Linux installer: an existing `/etc/arcen/xorg.conf` is repaired only when
  it is a regular root-owned file that group and other users cannot write, the
  rule the Pier itself enforces, and backups never overwrite an earlier one.
  The Pier's head-discovery probe now applies the same rule before starting its
  root X server.

## [0.16.0] — 2026-10-16

### macOS Deck

- Two Deck apps from one source, both for macOS 12 or later: **Arcen Deck**
  for Apple silicon (`Arcen-Deck-<version>-macOS.zip`) and **Arcen Deck
  Intel** for Intel Macs (`Arcen-Deck-Intel-<version>-macOS.zip`). They share
  the bundle ID `deck.arcen.tech`. `build-deck-app.sh --arch arm64|x86_64`
  builds either on either kind of Mac, so one release machine signs and
  notarizes both; every binary is checked for exactly one architecture and a
  `minos` equal to `LSMinimumSystemVersion`.
- `apple-cf` 0.9.3 and `videotoolbox` 0.18.1 are vendored with macOS 12
  patches (`third_party/*-arcen1`, each with `ARCEN_NOTICE.md` and a source
  manifest), so the Deck loads on macOS 12 and builds with Swift 5.7.
- A Deck platform profile (`arcen_media::deck_platform`) decides what differs
  between Apple silicon and Intel: the `deck_platform` label, AV1 only on
  Apple silicon with macOS 14 and only when the decode probe proves it,
  software decode expected on Intel, and native tablet only on macOS 13+.
  `arcen-deck platform-report` prints it without opening a window.
- The viewer takes keyboard focus when nothing else owns it, so keys work
  before the first click.
- Paced frames age against the display link's current time rather than the
  future scan-out target; on Intel/AMD with macOS 12 a ~49 ms lead had marked
  every frame stale.
- Presented-handler callbacks count as presented until a drawable reports a
  `presentedTime`, so graphics stacks that never timestamp are not reported as
  unpresented.
- The Deck adds the system Swift rpath to its own binaries, so native builds
  with the macOS 12 SDK load the Swift runtime.

### Security and supply chain

- `rustls` 0.23.45 and `rustls-webpki` 0.103.15.
- `scripts/verify-vendored-source.sh` checks every vendored tree before each
  Deck build and in CI, and refuses symlinks or any entry that is not a regular
  file or directory; the `apple-cf` Swift bridge never follows symlinks.

### Input

- Pen and tablet support is no longer tied to Wacom or to any list of tablet
  makers. Whether a device is a pen tablet is decided from its HID report
  descriptor by a new shared, bounded classifier
  (`arcen_usb_bridge::classify_pen_tablet`): a standard Digitizers-page pen
  with position and contact, pressure or proximity qualifies; so does a
  Digitizer or Pen collection on the vendor-defined Digitizers page `0xFF0D`
  (as Intuos5-style tablets publish it, with opaque reports and unknown axes)
  unless the device also has a game-controller collection; keyboards, mice,
  gamepads, devices on other vendor pages and touch-only devices are refused
  on their own. Express keys presented as a keyboard or keypad are part of a
  pen tablet and are accepted with it.
  Vendor ids are now informational only.
- macOS Deck: Tablet support is offered for any pen tablet, USB or Bluetooth,
  whether or not one is attached when connecting, replacing the
  Wacom-USB-vendor-id check that left other tablets without typed pen input.
  Tablet presence (report descriptors in the I/O Registry, or a pen sample
  AppKit already delivered) is shown in the Tablet Monitor. Settings, help, Tablet Monitor, logs and errors
  say "tablet" instead of "Wacom".
- Pressure, tilt and rotation conversions are shared
  (`arcen_input::pen_axes`). The Linux virtual pen's `ABS_PRESSURE` range is
  now `0..=65535` instead of Wacom's `0..=8191`; Windows keeps the Windows Ink
  `0..=1024` API range through the same shared scaling.
- macOS Pier: the virtual pen's tablet-proximity vendor id is `0xFFFF`
  (reserved, Arcen's id for synthetic devices) instead of Wacom's.
- Native tablet (USB bridged), lab builds: the privileged USB helper captures
  any attached device whose report descriptors classify as a pen tablet. It
  reads them from the I/O Registry rather than opening every HID device, and
  sends a USB request only for an undescribed interface of a registry-proven
  digitizer without a boot keyboard. It sends those descriptors with the
  device facts
  (`UsbHardDeviceMsg.hid_report_descriptors`, additive). The Linux Pier
  classifies them again before creating the `usb-vhci` device (replacing its
  single Intuos5 identity allowlist) and checks every descriptor the device
  later returns against the authorization: device, configuration and report
  descriptors must keep the authorized class, identity, HID-only interfaces
  and report bytes, or the attachment ends. The whole tablet is bridged,
  express keys (keyboard/keypad interfaces) included: the authenticated Deck
  can already send any keystroke, so this grants nothing new. A device with a
  joystick, gamepad or multi-axis controller collection at any nesting depth,
  or with no pen digitizer, is never bridged; a configuration must advertise
  each interface's full claimed report-descriptor length and the device must
  serve it whole, and claimed descriptors are bounded to 12 KiB so the helper
  never seizes a device it cannot describe. **Compatibility:** a 0.15 Deck
  sends no descriptors, so a 0.16 Linux Pier refuses its Native tablet
  request with an explicit reason; Tablet support is unaffected.
- Experimental raw-HID passthrough (quarantined, default off): the Deck
  matches digitizer collections by usage page and descriptor instead of five
  vendor ids, and the Linux Pier admits devices through
  `arcen_usb_bridge::RawHidAdmission` instead of a vendor allowlist.

### Protocol

- Rust names are vendor-neutral (`TabletModeMsg::UsbTabletBridge`,
  `TabletModeCapabilitiesMsg::usb_tablet_bridge`, `arcen_input::TabletMode::
  UsbTabletBridge`), but the wire keeps the 0.15 token and key
  `wacom_usb_bridge` (serde rename), so 0.15 and 0.16 peers interoperate
  unchanged. The neutral `usb_tablet_bridge` is accepted on input only. See
  `shared/protocol/WIRE.md`, "Tablet mode split".

## [0.15.0] — 2026-10-06

### Streaming

- macOS Pier advertises host audio again for signed-in sessions. Asking for
  helper permissions in turn (28 Sep) replaced the only call that marked audio
  available, so every hello said `audio: false` and the Deck never requested
  sound; it is now set on every agent and serve path before any prompt.
- Pier debug logging now makes Keel QP-map effectiveness provable live:
  Windows and Linux capenc inherit the Pier debug profile for helper
  diagnostics, and Linux XShm/NVENC stats report per-second biased/neutral
  QP-map counts plus mean dirty fraction alongside the capture damage source.
- macOS Deck native 8-bit and 10-bit video layers now use a shared
  refresh-sequence pacer driven by the window's `CVDisplayLink`: FIFO depth is
  capped at three, overflow drops the oldest frame, stale frames are discarded
  after coarse age caps, each submitted drawable uses a half-refresh minimum
  display duration, and telemetry separates SUBMITTED cadence from
  nonzero-`presentedTime` confirmations while reporting overflow/stale/hidden
  drops, source-cadence underruns, queue depth, refresh gaps and ESTIMATED
  receive-to-callback latency.
- macOS Deck no longer discards a paced frame when every drawable slot is
  still in flight: the refresh waits (`slot_waits` telemetry) and the frame is
  shown on the next one. A full one-refresh minimum display duration made
  presents miss their vsync and cost about one frame in four on a 60 Hz panel.
- The Deck video inbox now holds ~500 ms (30 packets) for every pipeline, so a
  Wi-Fi/VPN stall's burst is decoded instead of cleared; the old 8-packet inbox
  turned 250-300 ms stalls into a repeating keyframe loop.
- Windows Pier QP maps now use OS damage on the NVENC GPU path: Desktop
  Duplication dirty/move rects and WGC dirty regions feed Keel without CPU
  readback, and logs truthfully report `damage_source` plus disengage when no
  damage source exists.
- Windows Pier multi-monitor topology now programs rotated CCD outputs with
  oriented source surfaces and native target timings, so mixed
  landscape/portrait Deck layouts keep truthful READY geometry and no longer
  ask Windows to apply an impossible portrait target mode.
- Streaming pipelines now own a shared keyframe policy: hardware encoders stop
  inserting two-second periodic IDRs and rely on Deck-requested recovery, while
  software fallback keeps a longer bounded safety refresh.
- macOS Pier now honours `redirection.timezone` without adding a second helper:
  the launchd-managed Agent Helper validates the Deck IANA time zone against
  zoneinfo and sets `TZ` in its own GUI session, so newly launched apps inherit
  it. The state lives in that session (an owner-tagged `ARCEN_SESSION_TZ`
  sentinel), is restored on session end and SIGTERM, and dies with logout. A
  user-defined `TZ` is left alone. Upgrades remove the earlier root timezone
  helper and restore any system time zone it changed.
- Pipeline queue contracts now distinguish raw capture overflow from encoded
  access-unit loss: raw frame pressure is latest-wins and does not request an
  IDR, while encoded packet loss keeps the shared rate-limited keyframe
  recovery path. Windows, Linux and macOS hosts report the relevant drop
  counters without noisy per-frame diagnostics unless debug logging is enabled.
- Deck keyframe requests that arrive inside a Pier's short request guard are
  now coalesced and delivered when the guard expires instead of being dropped
  (Linux and macOS Piers; Windows already forwarded every request), and a
  recovery handoff that fails is retried on its own deadline.
- The Deck now normalizes host colour metadata through shared media before
  deciding presentation retags, so equivalent default and explicit BT.709 tags
  no longer reconfigure the presenter.
- Secondary Deck monitor windows now paint a neutral waiting placeholder before
  their first frame instead of a black surface; retained frame replay remains
  the preferred path when a frame is already available.
- macOS Pier now installs a Core Audio HAL input driver named `Arcen Microphone` and, when `microphone_input.enabled` is true and the driver Mach service is reachable, accepts Deck microphone-v1 Opus/PCM frames into that virtual 48 kHz mono Float32 input device.
- Windows Pier now makes the display HDR toggle follow the served pipeline:
  HDR sessions enable HDR on the exact capture targets, SDR pipelines disable
  it there before capture, and session/journal recovery restores the user's
  original state.
- Windows Pier NVIDIA headless HDR sessions now arm one recovery journal before
  HDR and exact timing changes, record temporary NVAPI timing ownership in that
  journal, and only record the held display after HDR is active and the exact
  mode is re-applied.
- Packaged Pier config templates now enable QP maps for Auto and Speed
  (`{"auto":"on","speed":"on"}`) while the built-in default remains off and
  existing installs keep their local config.
- macOS Deck `dev-tools` builds now support an env-gated windowed
  multi-monitor live-session test mode. `ARCEN_DECK_WINDOWED_MONITORS=2|3|4`
  requests a real multi-monitor layout from the Pier but presents each
  negotiated monitor as a decorated, resizable, non-fullscreen tiled window on
  one local display; production builds do not compile the mode.
- macOS Deck multi-window sessions now replay retained secondary frames when a
  secondary window becomes ready, aspect-fit secondary video with black
  letterboxing while routing input through the fitted image rect, and wrap
  display mismatch notices inside narrow viewer windows.
- macOS Deck multi-window input now ignores secondary-window clicks that start
  in letterbox bands while still delivering releases for drags that began
  inside the fitted image.
- macOS Deck windowed multi-monitor test mode now keeps the root window's
  stable title so native video layer lookup still works, and release packaging
  refuses any binary carrying the `dev-tools` marker even on `--no-build`.
- Pier configuration now accepts `video.qp_map` as either one
  `off`/`neutral`/`on` policy or a per-served-pipeline object keyed by Auto,
  Speed, Grading, HDR, Software and Custom. Linux and Windows pass the
  effective served-pipeline policy to capenc; macOS accepts the key and logs
  that VideoToolbox has no QP-map support.
- Software-only Pier refusals for Grading/HDR now carry the shared user-facing reason to the Deck instead of collapsing to a generic capture/encoder setup failure, while internal init failures remain generic.
- Linux multi-monitor sessions now aggregate the served pipeline across every started monitor encoder, so an NVENC primary with an OpenH264 fallback secondary reports and runs the Software contract instead of advertising the primary hardware preset.
- macOS Pier multi-monitor sessions now apply served-pipeline encoder bitrate start/ceiling per region and run one adaptive session rate controller from QUIC path signals, pushing live bitrate targets up to each region's operational contract ceiling without forcing idle re-encodes.
- The macOS Deck now latches native 8-bit and 10-bit presenter failures to the
  RGBA fallback for the rest of the session, and keeps recovery requests armed
  only until a non-empty RGBA fallback frame is handed to the existing texture
  path, so occluded windows do not keep idle streams polling forever.
- The macOS Deck now presents Grading and HDR root sessions through the same dedicated presenter-thread model as Auto/Speed instead of driving native video from egui repaint passes. Grading remains native `xf44` HEVC Main 4:4:4 10 BT.709 SDR into an RGB10A2 Metal layer with EDR off; HDR remains PQ/BT.2020 into that layer with EDR/HDR10 metadata only when the host confirms PQ.
- HDR now keeps the conservative link-safe start but owns a 500 Mbit/s contract
  ceiling through the same shared encoder-ceiling path as Grading, plus an
  evidence-gated fast probe and HDR-scoped host/Deck queue budgets so other
  pipelines keep their existing limits.
- NVENC sessions now initialise the encoder with the resolved session frame
  rate instead of a nominal 60 fps, so 30 fps Linux and Windows sessions deliver
  their configured bitrate target rather than roughly half of it.
- Speed now keeps the proven safe 1080p30 bitrate start but owns the true
  60 fps clean-path ceiling in the shared pipeline contract. Hosts derive
  Speed's motion priority and one-frame low-latency encoder/decoder policy
  from the served pipeline contract: NVENC reports P4/ultra-low-latency,
  zero reordering, no lookahead and a one-frame VBV; macOS ScreenCaptureKit
  captures at 1/60 for Speed requests; and the Deck applies VideoToolbox
  real-time decode for Speed sessions without treating optional decoder tuning
  as fatal.
- Auto, Speed, Grading and HDR are now named in the authenticated video request
  and echoed in `server_hello` as the active pipeline. The shared media crate
  owns one data contract per pipeline (codec ladder, colour target, bitrate
  bounds, keel policy and degradation text), while legacy clients still resolve
  through the previous request-field inference.
- Grading keeps the conservative link-capped starting bitrate but now has a
  shared 250 Mbit/s fidelity ceiling. The host adapters carry an optional
  served-pipeline encoder ceiling into capenc generically, so future fidelity
  pipelines can reuse the same path. Linux and Windows NVENC pass the value as
  `maxBitRate` while sizing VBV from the active target; the macOS Pier keeps
  `AverageBitRate` on the active target and passes the ceiling through
  VideoToolbox `DataRateLimits`; Auto and Speed retain their previous encoder
  bitrate numbers.
- The Deck command line and smoke tools accept `--pipeline
  auto|speed|grading|hdr`, deriving the same request fields as the GUI presets
  and printing the host's active pipeline.
- Software fallback is now a governed VM pipeline: the shared Software contract starts CPU H.264 near 4 Mbit/s for 1080p30, lets the live rate controller climb toward an 8 Mbit/s-class ceiling, and both Linux X11/OpenH264 and Windows MF/OpenH264 helpers accept runtime bitrate updates. Speed requests are visibly served as Software/Auto at 30 fps; Grading and HDR are refused on software-only hosts.
- Windows NVENC (the H.264, HEVC and AV1 hardware path) now submits frames
  through Keel's idle cadence, like the Linux NVENC path: a new frame, a
  requested keyframe, one pipeline flush after a change, and otherwise a
  keepalive once a second. It used to re-encode the last frame at the full
  frame rate, so an idle desktop at 60 fps still cost about 4.8 Mbit/s. The
  gate (`arcen_keel::SubmissionGate`) moved from the Linux encoder into
  `shared/keel`, so both hosts run the same policy.

### Fixed

- macOS Deck sign-in hand-over now keeps the actual retained 8-bit or 10-bit
  Metal presenter and its picture evidence alive across reconnect startup, so
  the last login-window frame stays visible until the user's desktop sends a
  replacement frame.
- Linux Pier installer upgrades now use the shared certificate provisioning
  plan: only positively classified legacy Arcen self-signed host pairs are
  reissued over the existing key with existing SANs, shared ownership marker
  and pin files; operator self-signed or CA-issued PEM and mismatched markers
  are left untouched. Linux installer issuance also shares the helper's
  certificate lock, journal, backup and rollback publication model.
- Windows Pier no longer warns "writer stopped with error" when the Deck quits
  normally and the broker has already closed the agent IPC WebSocket; resets
  and timeouts still warn.
- macOS Deck: dedicated 8-bit and 10-bit CAMetalLayer presenters now apply
  fullscreen/resize geometry on the main thread, update backing-scale drawable
  size, and replay the retained frame after geometry changes so native video
  fills the resized viewport.
- macOS Deck: session windows now keep an opaque black Core Animation backstop
  below dedicated native video, so any transient layer gap shows black instead
  of the desktop behind the transparent window.
- Windows remote unlock no longer always waits the full 15 s post-login
  stability ceiling: after an exact console bind, the broker can launch early
  and the per-session agent waits until the target session's input desktop is
  `Default` before starting capture. Repeated exact-bind and WTS-topology
  diagnostics are deduplicated.
- The macOS Deck now reports raw host-clock frame age as `wire_clock_age_ms`
  and uses offset-free `wire_delay_ms` (raw age minus the session rolling
  minimum) for overlays and stream-delay summaries, so inter-machine clock
  offset no longer looks like wire latency.
- Startup ramp no longer poisons session health with false critical FPS:
  shared QoS assessment suppresses host and client FPS during the bounded Pier
  warm-up window while keeping loss and latency checks active. Idle-desktop FPS
  classification remains a known gap until per-monitor damage evidence is
  wired into the health sample.
- Manual or graceful session shutdowns no longer log normal Deck disconnects
  as media-worker errors or Windows writer/audio-barrier warnings.
- macOS Deck now declares its Local Network privacy reason and, when LAN-only advisory probes report the host or network unreachable, shows a Local Network/offline-host hint while connecting and reports that specific reason if the normal connect deadline expires.
- Windows Pier debug logging now records incoming QUIC attempts before handshake completion plus startup network/firewall diagnostics and WTS session topology at boot/session changes.
- macOS Deck: transient dedicated-presenter drops or occluded skips no longer
  cover an already-established 8-bit or 10-bit native video layer with a black
  session background; fatal fallback statuses still switch to the egui path.
- macOS Deck: single-window sessions now re-enable display-refresh pacing on
  the root presentation layer while preserving immediate root presents for
  multi-window sessions, reducing input-storm pressure on WindowServer.
- macOS Deck: session telemetry now reports per-interval dedicated-presenter
  transient drawable drops and occluded skips as `presenter_drops` and
  `presenter_skips`.
- Linux Software Pier: live OpenH264 frame-rate sync no longer forces an IDR,
  avoiding once-per-second keyframe pops during Detail-priority telemetry.
- Linux CUDA/NVENC AV1 now repeats the AV1 sequence header on forced
  keyframes, matching Windows NVENC so recovery IDRs are classified as
  self-contained instead of leaving the Deck waiting.
- macOS Deck reconnect and resume overlays now preserve the last dedicated
  8-bit or 10-bit native video layer picture instead of flashing black while
  the replacement session waits for a fresh keyframe.
- macOS Deck root-layer pacing and colour tagging now positively avoid both
  dedicated video layers, only keeps transparent backgrounds after the current
  native layer has presented a picture, and counts every presenter drop/skip
  even when status notifications coalesce. Session telemetry also reports
  `ui_layout_passes_per_s` beside `ui_passes_per_s`.
- macOS Deck native-only reconnect/resume now leaves the recovery overlay as
  soon as the replacement session publishes its first native video frame,
  instead of waiting for an egui fallback frame that never arrives on the
  dedicated 8-bit and 10-bit paths.

- Windows Pier: a session no longer fails with "expected client_hello during
  handshake, received path_signal" when the Deck is slow to send its hello.
  The service starts forwarding live path signals to the session agent as soon
  as it relays; the agent's handshake reader now passes over that service-only
  message instead of refusing the session (seen as "Connection reset without
  closing handshake" on the Deck).

- macOS Pier: the 8-bit low-latency VideoToolbox encoder (Auto and Speed)
  does not report whether it is hardware-accelerated, so every macOS
  Auto/Speed session was served as `custom` and the Deck showed a pipeline
  degradation. The low-latency session is now created requiring a hardware
  encoder, which proves its class; if no hardware encoder is available it is
  created without the requirement and classified from its own read-back.
- Deck CLI: `media-smoke` prints the host's authoritative `served_pipeline`
  updates and reports the latest served pipeline in its summary.

- macOS Pier multi-monitor sessions now send the authoritative
  `served_pipeline` truth before the first region-video frame, aggregating every
  region encoder's hardware/software read-back through `shared/media`; the
  macOS Deck applies both the served pipeline and the reported backend so the
  degradation badge no longer mixes software fallback truth with a provisional
  hardware hello.
- Windows Pier multi-monitor sessions now aggregate served-pipeline truth across
  every admitted monitor encoder instead of trusting only the primary monitor,
  so a hardware-primary/software-secondary set reports the software fallback.
- macOS Pier exact/custom single-monitor sessions preserve the absent requested
  pipeline through VideoToolbox encoder initialization, using Auto only for
  bitrate sizing and keeping the authoritative served truth `custom`.
- Linux, Windows and macOS Piers now size their operational rate controller
  from the pipeline actually served rather than the pipeline requested: HDR
  degraded to Grading uses Grading bounds, exact/custom streams keep legacy
  shape-derived bounds, and software fallback uses the Software contract.
- macOS Pier now reapplies the authoritative served contract after VideoToolbox
  reports the real encoder acceleration, updating the initial encoder bitrate
  and the controller before the first frame; multi-monitor region encoders apply
  the aggregate served contract as well.
- Windows Pier: a host whose display belongs to a virtual display adapter
  (for example a GeForce PC with a third-party virtual display driver) streams
  again. Every single-display NVENC session first asked NVIDIA to rebuild the
  display to the Deck's size; NVAPI does not drive such a display, so the
  session agent failed and the Deck saw only "Connection reset without closing
  handshake". When NVIDIA refuses before anything on the host has changed, the
  session now serves the existing display at its nearest mode. A failure while
  displays are being changed still refuses the session.
- Windows Pier: every failure before the session agent is ready now reaches
  the Deck as a reason instead of a reset connection.
- Windows Pier: the display recovery snapshot no longer requires NVAPI to
  know every output an NVIDIA GPU renders. A virtual display adapter's
  monitor is recorded as an ordinary Windows output, so the Pier may change
  its mode instead of refusing the session.
- Windows Pier: NVIDIA EDID provisioning is attempted only on Quadro, RTX Pro
  and GRID GPUs. A GeForce answered `NvAPI_GPU_SetEDID` with
  `NVAPI_NOT_SUPPORTED`, its rollback failed the same way, and every later
  session found an unrestorable recovery journal. A GeForce host is now
  served on its existing display without touching EDIDs.
- Windows Pier: a session no longer ends with "display restore failed" and an
  armed recovery journal when the display driver no longer offers the mode the
  display started in (a virtual display's default mode, or a mode list edited
  during the session). Windows refuses that mode every time, so the display is
  left at its current mode. Restores that also undo NVIDIA or VMware state are
  unchanged.
- Windows Pier: the capture log reports how often Desktop Duplication saw the
  pointer move, which shows whether a display draws the pointer into the
  picture (and a Deck drawing its own pointer then shows two).
- Windows Pier: when Desktop Duplication is refused, capture waits up to five
  seconds for it (the secure desktop shown right after a sign-in is usually
  brief) instead of settling at once for a WGC capture that may never deliver
  a frame, and the log names the input desktop it found, so a black picture
  caused by a lock screen or a pending UAC prompt says so.

### Performance

- The macOS Deck now has a dedicated root-window Auto/Speed presentation path:
  VideoToolbox's 8-bit 4:2:0 IOSurface-backed `CVPixelBuffer` is wrapped as
  Metal plane textures and drawn into a separate display-synchronised
  `CAMetalLayer`, so ordinary 8-bit video frames no longer require a CPU
  `CVPixelBuffer`→RGBA copy, `queue.write_texture` upload, or egui repaint.
  The existing egui/wgpu upload path remains the visible fallback, and
  secondary monitor windows still use the old path.
- The macOS Deck no longer runs its UI flat out on a still desktop. A session
  ran a full UI pass every 16 ms, and the media worker woke the UI again for
  every media batch, about 50 times a second for audio alone. Each pass
  repaints the whole window, so a full-screen Deck on a 3024×1964 panel showing
  a 1 fps idle stream measured about 48% CPU plus about 53% in WindowServer.
  With the window hidden, eframe skips the paint but not the pass, and the
  Deck spun at about 95%. The session now polls every 250 ms when idle, and
  every 16 ms for one second after local input or during a reconnect.
  Decoded frames, UI-affecting host results (cursor shape and mode, tablet
  mode, display updates), and tablet and gesture samples each wake the UI
  themselves.
- A healthy session no longer rebuilds its reconnect identity on every UI
  pass. The identity enumerates every display through WindowServer (about
  0.4 ms in the Deck plus about twice that in WindowServer per pass), and the
  auto-reconnect controller exists for the whole session, which also kept the
  idle poll above at 16 ms. Both now apply only while a resume is under way.
- `session telemetry` now carries `deck_cpu_percent`, `ui_passes_per_s` and
  the last `wire_video` header (codec included), so a Deck log shows what each
  session costs the Mac it runs on.

## [0.14.0] — 2026-09-28

### Configuration

- One set of defaults on every host, with everything on: the Linux, Windows
  and macOS installers write identical common sections (10-bit video, Opus
  audio, microphone input, clipboard both ways, time-zone redirection), and
  `check_shared_contracts.py` fails if they drift. The file is for turning
  things off. Multi-display is on where the host can prove it (macOS always,
  Windows by startup selection from live DXGI/NVAPI inventory, Linux by
  service-start NVIDIA head discovery). Existing configurations are kept on
  upgrade.
- Windows: multi-display no longer depends on an installer-time allow-list
  probe. Empty `platform.multi_monitor.allowed_adapters` means any eligible
  NVIDIA/NVENC adapter, `excluded_adapters` reserves GPUs such as an RTX card
  for other work, and NVIDIA headless provisioning is automatic unless an
  administrator forces it on or off.
- Linux multi-monitor now discovers NVIDIA `DFP-N` heads automatically from a
  short Xorg probe and ranks them by maximum pixel clock. Empty
  `platform.multi_monitor.heads` means automatic discovery; a non-empty list is
  an administrator override, and `advertise_enabled: false` remains the off
  switch.
- Windows: time-zone redirection works; the installer now creates the
  recovery directory its crash journal needs.
- macOS: the unused `virtual_display_enabled`, `native_login_enabled` and
  `privileged_broker_enabled` settings are no longer written, and no longer
  log a misleading "refusing session admission" warning.

### Shared rule, enforced

- `scripts/check_shared_contracts.py` (CI and pre-commit) fails when a Pier
  stops using a shared policy it must use, when a known host-local copy of a
  shared policy reappears, or when new portable code (no OS API) lands under
  `hosts/`. `check-workspace-boundaries.sh` works again and proves the
  dependency direction with a reviewed allowlist of product helper crates.
- Every Pier drives the shared session lifecycle, and every installer the
  shared install transaction.
- The Linux and Windows video queues share one drop / keyframe-recovery
  policy; macOS uses the shared audio priority stream.
- Every host takes the Deck display's colour from the shared rule; a bright
  display without HDR headroom is no longer counted as HDR.

### Installers

- An install succeeds only when the Pier is proven running: the Linux and
  Windows installers exit non-zero when the service does not settle running,
  and the macOS package fails when the service does not listen or a signed-in
  user's agent does not start. A missing privacy approval is onboarding, not
  a failure.
- Linux: after an upgrade without `--restart`, the running Pier keeps
  accepting new sessions on its own build until it is restarted, instead of
  refusing every login.

### Streaming

- macOS Pier now honours `redirection.timezone` without adding a second helper: the existing Agent Helper validates the Deck IANA time zone against zoneinfo, sets `TZ` in its launchd GUI session so newly launched apps inherit it, and restores the previous value from a per-user crash marker.
- The encoder bitrate follows the QUIC path on every Pier through one shared
  controller (`arcen_media::rate_control`). A random loss floor on Wi-Fi,
  cellular and VPN paths is not counted as congestion, a congestion epoch
  cuts once rather than every second, and a multi-display session is
  measured across all of its monitors and reaches every encoder pipeline.
  `ARCEN_RATE_CONTROL=0` restores the fixed rate.
- Presets are stated trade-offs, not promised frame rates: when an encoder
  cannot sustain a preset's ceiling (for example Speed at 60 fps on two
  displays of a vGPU), the session is admitted at the best proven rate and
  says so (`degradation_reason: fps_reduced_by_encoder_capacity`) instead of
  being refused.
- Every Pier now uses QUIC's BBR congestion controller, which does not read
  random loss as congestion. On a VPN path with a few tenths of a percent of
  random loss it gave each of two displays 2.5–3× the bitrate at half the
  latency of Cubic, and it matched Cubic on a calm path.
  `ARCEN_QUIC_CONGESTION=cubic` restores Cubic (Quinn marks its BBR
  experimental).

### Match my displays

- Deck: with several displays the remote pointer could bounce between
  displays about a thousand times a second (the Windows Pier lost the mouse,
  the Linux Pier lagged); a display now moves the pointer only when its own
  input carries a pointer event. A button released outside the display it
  was pressed on is still delivered.
- Windows Pier: the same user reconnecting takes over the session at once
  instead of waiting out the resume window; a different user is still
  refused. A new session waits for the previous one's display restore to
  settle, and every session end rolls back the NVIDIA headless outputs it
  provisioned.
- macOS Pier: each session owns its virtual displays through a short-lived
  helper process, so repeated multi-display sessions keep working.

### Input

- Trackpad scrolling is precise end to end: phased point deltas on macOS,
  high-resolution wheel on Linux, fractional and horizontal wheel on Windows.
- Negotiated gestures (`gestures_v1`): the Deck captures magnify, rotate,
  smart zoom and swipe; the macOS Pier injects what macOS allows publicly and
  declines rotate without ending the session.
- macOS Pier: reports the cursor and tablet modes it applied.

### Performance

- Windows Pier: colour conversion runs on the GPU in every preset. Eight-bit
  capture goes to NVENC as a texture (conversion 19.5 ms → 0), and Grading and
  HDR convert FP16 scRGB in a compute shader (about 14 ms and 38 ms per frame
  → under 0.1 ms), with the stream truth unchanged. The CPU path remains an
  explicit, logged fallback.
- macOS Pier: no audio dropped on a calm path (was 14%); VideoToolbox runs
  with low-latency rate control.

### Displays

- macOS Pier: a Deck with several displays gets one virtual display per Deck
  display, arranged as the Deck's layout (including offset arrangements),
  with region input.
- Deck: a launch-time quick connect negotiates Match My Layout like the Home
  screen, and a saved connection's own Displays choice applies to it. The
  Deck's root window sits on its fastest display, and each display presents
  at its own refresh. Known issue: with several displays the Deck's draw loop
  runs far more often than frames arrive, which costs CPU.

### macOS Pier installer

- The first page says what is installed and why it is more than one app: a
  network service running as the hidden `_arcen` account, and a helper in
  each signed-in session. The full reasoning is public in
  `docs/security/macos-pier-process-model.md`.
- The Agent Helper moves to `/Library/PrivilegedHelperTools`, so
  Applications shows one Arcen app. The package no longer asks for a
  destination or install location.
- Both launchd jobs declare `AssociatedBundleIdentifiers`, so System
  Settings attributes them to Arcen Pier.
- The helper asks for its three approvals as soon as it starts, one at a
  time: system audio, Accessibility, then Screen Recording. Previously
  Accessibility opened underneath Screen Recording, and system audio waited
  for the first Deck.
- Declining "Installer would like to administer your computer" (the
  `_arcen` account) stops the install with a reason instead of leaving a
  service that cannot start.
- `uninstall.sh --purge` also resets the privacy approvals given to Arcen.

### Fixed

- Deck: *Forget and Verify* on a changed host identity reconnects at once and
  shows the new fingerprint, reusing the password already given, instead of
  returning to Home.
- Deck: a host's reason for closing a session is shown rather than a bare
  "Host closed the session"; the macOS Pier sends one when it refuses a
  session after sign-in (for example while system audio is unapproved),
  instead of resetting the connection.
- Deck: a host that accepted the sign-in but could not start the session says
  so ("the host signed you in but could not start the session") instead of
  "authentication failed" (`AuthResult.session_setup_failed`).
- macOS Pier: advertises its build identity like every other product, from
  one shared `arcen_protocol::build_identity`.
- Linux installer: an installed but stopped firewalld is reported as such,
  not as a failed firewall update.
- macOS Pier: the relay no longer sends path signals to the agent before the
  Deck has authenticated, which could fail the handshake; a Deck can no
  longer send service-only messages to an agent on macOS or Windows.
- Windows installer: waits for the service as long as the service control
  manager does (30 s) before reporting a failed install.
- Windows Pier: the adaptive bitrate task stops with its attachment and
  follows replaced pipelines; a fatal attachment cleanup still ends the
  session's shared lifecycle.

### Linux Pier

- On a desktop declared `rec2100-pq`, Grading is converted to true BT.709 SDR
  and eight-bit presets are refused with a clear message.

## [0.13.0] — 2026-09-27

The first release with a macOS Pier package, Linux HDR, and hosts that follow
the Deck's displays. Every streaming preset was measured end to end on the lab
hosts with the Deck GUI.

### macOS Pier (preview)

- A working host. A `_arcen` network service (LaunchDaemon: UDP 18444, TLS,
  admission, relay) and a per-session desktop agent (capture, encode, input,
  clipboard, audio) share the `arcen_session::agent_relay` contract.
- The login window streams, and the Deck follows sign-in into the desktop.
  Input at the login window uses the entitled virtual HID keyboard and
  absolute pointer (`com.apple.developer.hid.virtual.device`).
- Speed, Grading and HDR are separate pipelines, proven in the bitstream.
  Grading is RExt 4:4:4 10-bit BT.709; HDR is RExt 4:4:4 10-bit PQ/BT.2020,
  claimed only on proven EDR headroom, with SDR white at 203 nits.
- A signed and notarised installer package with sysadmin-style install and
  `uninstall.sh --purge`. Privacy permissions must be approved at the Mac, so
  the command-line `installer` is refused by default.
- Still preview: no cold-boot LoginWindow path, and multi-monitor is not
  hardware-qualified.

### Deck

- Each screen's colour facts (P3 gamut support and EDR headroom) are sent to the
  host, which builds its virtual display from them. HDR is greyed out ("No
  HDR-capable screen found") when no screen can show it.
- Command shortcuts reach a macOS host (the Cmd→Ctrl swap is off for Aqua).
  Holding ⌘Q for 3 s quits the Deck, as in Chrome; a tap goes to the host.
- Logs the received stream truth (SPS), a per-plane distinct-code census, and
  a "stream delay" record with video and audio age at p50 and p95.

### Linux Pier

- Added `video.desktop_encoding`. `rec2100-pq` declares that a colour-managed
  application (Flame's HDR UI) writes Rec.2100 PQ into the depth-30 Xorg
  desktop, so an HDR request is passed through as PQ / BT.2020 instead of
  resolving to Grading. The default `sdr` keeps the previous behaviour.
- A session's virtual head is the Deck's display, by per-session EDID.
- Audio travels on its own priority QUIC stream, and the frame pump applies
  backpressure instead of requesting keyframe storms.

### Windows Pier

- HDR EDIDs carry the Deck display's gamut and luminance, and HDR places SDR
  white at 203 nits, as the macOS Pier does.
- Audio travels on the shared priority stream; the FP16 scRGB conversion uses
  up to 16 workers, bringing HDR to its target frame rate.
- `video.desktop_encoding` is rejected unless `sdr`: Windows reads HDR state
  from the operating system.

### Shared

- `DisplayColorMsg` and `arcen_media::display_color`; EDIDs carry gamut
  chromaticity and HDR luminance.
- `link_capped_average_bitrate_bps` is the one bitrate rule for every Pier.
- `PriorityAudio` QUIC stream in `arcen-transport`.
- `hevc_sps` stream truth, `pq_white` 203-nit contract,
  `DesktopSignalEncoding` / `constrain_to_desktop_encoding`.
- A host that has not measured its encoders follows the client's codec.

### Fixed

- Windows: an upgrade needs no flags and no second run. A self-signed key
  pair from an older install is taken over automatically on every host,
  keeping the key (a CA-issued certificate is served as it is and never
  reissued); an access entry Explorer added to `ProgramData\Arcen` is removed
  instead of failing the install; uninstall while the sign-in screen still
  has the credential provider loaded sets it aside and finishes; the
  transcript shows what changed, with command output behind `--verbose`.
- Windows: a host without NVIDIA (VMware, Citrix, Proxmox and other VMs)
  serves every preset as OpenH264 H.264 4:2:0 8-bit instead of failing the
  session after sign-in, and a failure before streaming reaches the Deck with
  its reason instead of as a dropped connection.
- The Linux and Windows Piers build again (missing hello fields).
- The shared codec resolver no longer turns Grading into H.264.
- The Linux and Windows capture encoders no longer overshoot the bitrate cap.

## [0.12.0] — 2026-09-12

### macOS Pier (new, in progress)

A native macOS host now exists as `hosts/macos`. It authenticates through PAM,
serves QUIC on UDP 18444 with its own certificate lifecycle, captures and
encodes the desktop through ScreenCaptureKit and VideoToolbox, injects pointer
and keyboard input, carries typed pen events, sends host audio when consent
exists, and carries the clipboard. It is not yet a complete Pier: there is no
published installer, no cold-boot LoginWindow path, no native USB tablet, and
multi-monitor is implemented but not hardware-qualified.

- **The real Deck now completes an authenticated session against it.** Three
  defects each ended every real session and none was caught by the tests,
  because host and test were both hand-written JSON and so agreed with each
  other rather than with the client: `server_hello` was sent before
  `auth_request`, which selects the Deck's no-authentication path and zeroizes
  the password; the password was read from a `password` field when the Deck
  sends it in `credential`; and `server_hello` omitted `negotiated_transport`,
  which the Deck requires, while misspelling `supports_h265`. All three are
  fixed by building the shared message types instead of hand-writing JSON.
- Clipboard now uses the real framed protocol. The host had invented a
  text-offer-then-bare-bytes scheme that no Deck speaks; it now sends and
  receives `FrameType::Clipboard` chunks through the shared
  `ClipboardReassembler`, and dispatches binary messages by frame type instead
  of treating every one as a clipboard payload.
- Host audio capture with a mandatory local-mute lease, built on public Core
  Audio process taps. Real audio is captured while the host's own speakers stay
  silent, which is the point of the feature.
- Host audio negotiation no longer deadlocks on itself. The host advertises
  from the Screen & System Audio Recording grant, publishes `audio_output`,
  sends `audio_stream_result`, and starts capture on a separate thread under a
  1500 ms budget. If consent is missing or Apple's prompt blocks inside tap
  creation, the Deck is told `CaptureUnavailable` and video continues.
- The macOS multi-monitor wire path is present: auth-time offer/request,
  applied topology in `server_hello`, and region video headers carrying monitor
  id, topology generation, and stream epoch. It is not claimed as proven:
  physical multi-display qualification remains outstanding.
- `packaging/macos/build-pier-app.sh` now builds the release Pier binary before
  bundling it, instead of packaging an arbitrary stale file from
  `target/release`.

### Shared

- `arcen-media` gained `clipboard::policy_message`, moved out of the Linux host
  before a second copy was written for macOS, and `audio::PcmPacketizer`, which
  frames arbitrary capture buffer sizes into fixed audio-v1 packets.
- `arcen-session` gained `audio.local_playback`, defaulting to `muted`. It is
  deliberately separate from `audio.enabled`: someone beside the host machine
  hearing the remote user is a privacy failure whether or not audio is being
  transmitted. Existing configurations parse unchanged and acquire the safe
  default.

### Fixed

- **Windows certificate renewal replaced the key it promised to preserve.**
  The migrated provisioning path printed "reissuing the TLS certificate" for
  renewal and adoption, then generated a new key anyway. Automatic renewal near
  expiry would have changed the host identity and broken every SPKI-pinned
  Deck, without the warning a deliberate rekey prints.
- **Linux `new-host-cert` produced certificates with no subject alternative
  name.** It parsed `--dns` and `--ip` and discarded them, so every Deck
  rejected the result during the TLS handshake while the material looked
  correct on disk. The shell helper in `packaging/linux/` has always done this
  properly.
- macOS: a rejected QUIC handshake ended the listener, so one wrong-ALPN
  connection could take a host offline; closing a stream could deadlock the
  serving loop; two seconds of desktop idleness ended a session on a
  damage-driven capture API; the Core Audio callback allocated on the
  real-time thread; failed audio teardown was recorded as success, which can
  leave a Mac silent; any authenticated account was served the console user's
  desktop; clipboard queues were unbounded; and `serve --once` reported
  success after a stream failure.
- `PcmPacketizer` accepted a frame specification rounding to zero samples,
  where `push` emits empty packets forever without consuming input.

### Validated

Linux (Rocky 9.5, 731 tests) and Windows 11 (688 tests) were built and tested
on their own operating systems after every shared change, rather than
cross-checked from macOS. The real Deck completes an authenticated session
against the macOS Pier.

## [0.10.0] — 2026-09-01

### Streaming presets and pipeline separation

- Replaced independent performance/colour controls with four complete product
  presets: **Auto**, **Speed**, **Grading**, and **HDR**.
- Kept the fast 8-bit and fidelity pipelines separate. Auto/Speed do not pay
  the host-copy or conversion cost required by Grading/HDR.
- Extended negotiated session truth with primaries and transfer degradation so
  the Deck can distinguish ten-bit SDR from real HDR and show every fallback.

### Windows

- Added a genuine ten-bit Grading source: WGC
  `R16G16B16A16Float` scRGB converted to full-range BT.709 I444 P16 before
  NVENC. FP16 refusal fails closed instead of repacking BGRA8 as ten-bit.
- Added end-to-end HDR: session-scoped NVIDIA HDR EDIDs, exact topology,
  Windows 11 distinct HDR state, DXGI PQ/BT.2020 verification, WGC FP16
  capture, linear-primary conversion, 80-nit-reference absolute ST 2084, and
  HEVC 4:4:4 10-bit output.
- Scoped HDR state changes to the exact session display targets and kept the
  pre-provision EDID recovery journal armed through lease teardown.
- Fixed forced-loss resume by atomically discarding buffered video before the
  writer is joined; credential-free reconnect now retains the desktop and
  returns to healthy media.
- Fixed Windows installer public ACL convergence on localized or previously
  installed systems.

### Linux

- Preserved NvFBC → CUDA → NVENC as the device-to-device 8-bit path.
- Added a separate depth-30 Xorg/MIT-SHM pipeline for genuine RGB10 capture,
  mask-derived `XBGR2101010` handling, shared P16 conversion, and one CUDA
  upload before NVENC.
- Made Xorg HDR requests resolve truthfully to Grading BT.709 SDR. Real Linux
  HDR remains gated on a future color-managed Wayland provider.
- Degraded host cursor authority to the Deck-local cursor only for the XShm
  wide path; NvFBC host cursor behavior is unchanged.

### macOS Deck

- Added native VideoToolbox `xf44` retention and a dedicated
  `RGB10A2Unorm` Metal presentation layer for Grading and HDR.
- Enabled ITU-R BT.2100 PQ, HDR10 metadata, and EDR only when the resolved host
  transfer is PQ. Normalized PQ output uses Apple's 10,000-nit optical scale.
- Preserved negotiated BT.709, sRGB, PQ, and HLG transfer metadata through
  VideoToolbox and made every 8-bit presentation fallback permanently visible.

### Release validation

- Completed Windows and Linux Auto/Speed/Grading/HDR matrices with decoded
  frames, nonzero audio, keyboard/pointer input, cursor authority, display
  restore, and forced-loss credential-free resume.
- Rebuilt the Linux and Windows single-file installers and the Developer ID
  signed, notarized, stapled macOS Deck.

## [0.9.8] — 2026-08-25

**The first public release.** Arcen was developed privately and is published
here for the first time, as free software under the AGPL-3.0. There is no
earlier public version; the history before this point was private and is not
part of the public record.

This release is numbered 0.9.8 rather than 1.0.0 deliberately. All three product
crates build and pass their tests on their target OS, and the Linux Pier and
macOS Deck have carried real sessions — but interfaces may still move, and only two of the six host/client combinations are
implemented. 1.0 should mean more than "it compiles".

### Transport and trust

- **QUIC-only product transport** on UDP 18444, with TLS 1.3 at the transport
  layer. There is no TCP fallback in shipped binaries.
- **Certificate trust model** with five explicit modes: system CA, private CA
  bundle, trust-on-first-use pending, TOFU-pinned, and a development-only
  insecure mode that is **double-gated** and refuses to engage unless both the
  configuration mode and an explicit CLI flag agree.
- **TOFU pairing ceremony** — on first connection the Deck shows the
  certificate SHA-256, the SubjectPublicKeyInfo SHA-256, and the validity
  window, and offers cancel / trust once / trust and remember.
- **Certificate pinning** on both whole-certificate and SPKI digests, compared
  in constant time, persisted per saved connection with the pin kind, the time
  it was pinned, and an optional label.
- Host certificate generation at install time covering the machine's real names
  and addresses.
- **Session auto-reconnect** with a bounded reconnect window that holds the
  session slot rather than tearing the desktop down.

### Video

- **NVENC hardware encoding** for H.264 and HEVC, including **HEVC 4:4:4 10-bit
  full-range** for grading work, and 4:2:2 support on Blackwell-generation
  hardware.
- **AV1 encoding** via `rav1e` for a royalty-free path.
- **Software H.264** fallback via OpenH264, behind an opt-in feature so the
  default dependency graph stays free of the native build chain.
- **Colour fidelity** work covering 10-bit and 4:4:4 pipelines end to end.
- **Multi-monitor** capture and presentation with a shared output-provider
  lifecycle.
- **Retina / effective stream resolution** handling on the macOS Deck.
- Region-based screen update patching.

### Audio

- Opus audio compression for host-to-client audio.
- Microphone / audio input redirection from the Deck to the host session.

### Input and devices

- Keyboard, mouse, scroll, and region input.
- **Pen tablet input** with pressure support.
- **Hard USB (USB-over-IP) device passthrough**, one tablet per seat, with a
  privileged macOS helper on the client side. Linux hosts only.
- **Timezone redirection** so the remote session reflects the client's timezone.

### Hosts

- **Linux Pier** with a dedicated Xorg session model, PAM authentication, and a
  single fused multicall binary that embeds the capture, audio, input-helper,
  session-agent, and session-launcher subcommands rather than shipping separate
  executables.
- **Windows Pier** with an IDDCX virtual display driver and a Windows
  Credential Provider participating in the logon path, plus a console-ownership
  policy that refuses a remote sign-in when a local account holds the physical
  console.
- Self-contained installers for both platforms that lay out directories,
  generate the host certificate, register and start the service, and open the
  firewall port.

### Clients

- **macOS Deck**, a native client with its own decode and render path, saved
  connection bookmarks, and per-connection trust configuration.

### Observability

- A dedicated OS-free tracing and bounded-I/O runtime (`arcen-observability`)
  and a pure event contract crate (`arcen-telemetry`), with a conformance
  validator run in CI.

### Removed before publication

- **The entire commercial licensing system.** Arcen previously carried an
  offline, signed, node-locked licensing stack — roughly 12,000 lines across a
  shared crate, both host adapters, and an issuer tool. Under the AGPL it serves
  no purpose, and it was the only thing preventing the software from running.
  The single-session admission gate it contained was kept and re-homed, because
  that constraint is physical: a Pier drives one desktop.
  Removing it also dropped an entire cryptographic dependency subtree from the
  build — `ed25519-dalek`, `curve25519-dalek`, `ed25519`, `signature`, `pkcs8`,
  `der`, `spki`, `const-oid`, `base64ct`, and `fiat-crypto`.
- **Roadmap components not yet real**: the Windows Deck and the
  shared test-kit crate were removed from the published tree rather than shipped
  as dead code.
- **`arcen-session`'s opt-in `authoritative-session` state machine**, which
  depended on the licensing crate, went with it. The dependency-light
  restore-lease, deskside and direct-reconnect surfaces — the parts actually in
  use — are unaffected.

### Changed before publication

- **AGPL section 13 source offer is built into the programs.** Arcen is
  remote-access software, so people routinely interact with a Pier *over a
  network* rather than running it themselves. Both Piers now carry the licence
  and the source location in their startup banner (and the Linux Pier in
  `--version`), and the Deck carries it in its startup banner. A user who only
  ever receives a built binary can still find the corresponding source.
- **NVENC bindings are now clean-room.** The checked-in NVENC FFI bindings were
  bindgen output derived from NVIDIA's Video Codec SDK header, which cannot be
  redistributed. They are regenerated from
  [nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers) `n12.1.14.1`,
  the MIT-licensed clean-room header set, vendored under
  `third_party/nv-codec-headers/`.

  The vendored header is deliberately **API 12.1 — the same version the previous
  bindings targeted** — so this is a purely legal change with no behavioural
  difference and no increase in the minimum NVIDIA driver (530.41.03 on Linux).
  NVENC API 13.1 would require driver 610.0 or later (older drivers answer
  `NV_ENC_ERR_INVALID_VERSION`). Verified on a live GRID V100D: capture and
  encode initialise and run.

### Verified on real hardware

Everything below was built and tested on its target OS before release:

- **Linux Pier** — builds release; 710 Pier tests and 163 capenc tests pass.
  NVENC capture and encode verified live on a GRID V100D (driver 570.172.08):
  CUDA init, NvFBC capture, and `NVENC ready: 2560x1600 codec=h264`.
- **Windows Pier** — builds release on MSVC; 676 Pier tests, 192 capenc tests
  and 51 credential-provider IPC tests pass.
- **macOS Deck** — builds; 926 tests pass.
- **Shared crates** — 694 tests, strict Clippy clean.

### Known limitations

- Arcen is designed for **direct machine-to-machine connections on a trusted
  network**. It is not hardened for direct exposure to the public internet.
- Released binaries are not code-signed on every platform; expect Gatekeeper and
  SmartScreen prompts.
- CI runs a Linux-only, manually triggered gate. Platform builds are not
  verified automatically.
- macOS Pier, Linux Deck, and Windows Deck do not exist.

[Unreleased]: https://github.com/Aanerud/arcen_public/compare/v0.20.5...HEAD
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
