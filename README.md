# Arcen

Arcen streams a real desktop from a host computer, the **Pier**, to a Mac,
the **Deck**, over one encrypted QUIC connection on **UDP 18444**. The host
can run Linux, Windows or macOS; your applications stay there, and you see
and work on them from your Mac with its own keyboard, cursor and pen tablet.
Free software under **AGPL-3.0-only** ([licence](#contributing-and-licence)).

## Quick start: try it in 10 minutes

### What you need, and why

| You need | Why |
| --- | --- |
| A Mac with **macOS 12 or later** | It runs Arcen Deck, the client. There are two apps: **Arcen Deck** for Apple silicon, **Arcen Deck Intel** for Intel Macs. |
| A host with a desktop (next table) | It runs Arcen Pier, which captures that desktop and encodes it. |
| **UDP 18444** from the Mac to the host, on a LAN or VPN | The only port Arcen uses. The installers open it on the host's firewall where they recognise it. |
| An ordinary **user account on the host** | You sign in with it; Arcen keeps no user list of its own. |

| Host | Requirements |
| --- | --- |
| **Linux**, X11 | Rocky Linux 9 or 10 with Xorg and an **NVIDIA GPU** and driver (capture and encode run on it). |
| **Linux**, GNOME Wayland | Rocky Linux 9 or 10 with GNOME 40+ and any GPU with a DRM render node. NVENC is used on NVIDIA; other GPUs get software H.264 (up to 30 fps). Grading and HDR need GNOME 50+. |
| **Windows** | Windows 10 1809+, 11, or Server 2022+. Grading and HDR need an NVIDIA GPU; without one the Pier encodes H.264 in software. |
| **macOS** (preview) | **macOS 14.2 or later**, with a user **signed in** at the console (the preview serves that signed-in desktop). |

### Steps

1. **Download** from [Releases](https://github.com/Aanerud/arcen_public/releases):
   the Deck for your Mac — `Arcen-Deck-0.19.0-macOS.zip` on Apple silicon,
   `Arcen-Deck-Intel-0.19.0-macOS.zip` on an Intel Mac (Apple menu → About
   This Mac shows which) — and the Pier installer for your host. Check them
   against the release's `SHA256SUMS.txt`:
   `shasum -a 256 -c SHA256SUMS.txt --ignore-missing`.
2. **Install the Pier on the host** (one command; it creates the service, a
   TLS identity and the firewall opening, and starts the Pier):
   - Linux, as root:
     ```sh
     chmod +x install-arcen-pier-0.19.0-linux-x86_64
     sudo ./install-arcen-pier-0.19.0-linux-x86_64
     ```
     `--check` first shows what it found and would do, and changes nothing.
   - Windows, in **PowerShell as Administrator**, then **reboot** (Windows
     loads Arcen's sign-in provider only at boot):
     ```powershell
     .\install-arcen-pier-0.19.0-windows-x64.exe
     ```
   - macOS: open `ArcenPier-0.19.0.pkg` in **Installer.app** on that Mac
     (not the `installer` command), then, signed in as the account that will
     be served, approve **Screen & System Audio Recording** and
     **Accessibility** for **Arcen Agent Helper**, and **System Audio
     Recording** when the helper asks. The last one is a separate list:
     without it the host refuses sessions, because it cannot keep its own
     speakers silent.
3. **Open the port if the installer could not.** For example on a Linux host
   with firewalld: `sudo firewall-cmd --permanent --add-port=18444/udp && sudo firewall-cmd --reload`.
4. **Note the host's identity fingerprint** (SPKI SHA-256), to compare in the
   next step: Linux `/etc/arcen/host.spki-sha256`, Windows
   `C:\ProgramData\Arcen\tls\host.spki-sha256`, macOS
   `/Library/Application Support/Arcen/tls/host.spki-sha256`.
5. **Install the Deck:** unzip it and drag **Arcen Deck** (or **Arcen Deck
   Intel**) to Applications, then open it.
6. **Connect:** add the host by address (for example `203.0.113.10` or
   `pier.example.internal`; the port is 18444). The first time, the Deck
   shows the host's fingerprint: if it matches step 4, choose **Trust and
   Remember**. Sign in with your account on the host.

**What you should see:** the host's desktop fills the Deck's window or screen,
your Mac's cursor moves on it, and typing reaches the host. The stream
starts in the Auto preset.

**If it doesn't connect:** use **Test connection** in the Deck, or run on the
Mac (Intel: `Arcen Deck Intel.app`):

```sh
"/Applications/Arcen Deck.app/Contents/MacOS/arcen-deck" diagnose 203.0.113.10 18444
```

It checks the network, the port and the host's identity without your
password and says what to fix. Then see
[Troubleshooting](docs/operations/troubleshooting.md).

## Optional extras

- **Native tablet:** use the pen tablet on your Mac's desk as if it were
  plugged into the host, with the host's own tablet driver. The Deck needs
  **macOS 13 or later** (on Intel Macs it has not been tested with a real
  tablet). On a Linux host it is off until you enable it, which restarts the
  running Pier:
  ```sh
  sudo ./install-arcen-pier-0.19.0-linux-x86_64 --native-tablet --restart
  ```
  On an X11 host, also restart the display manager once while nobody is
  signed in at the console (`sudo systemctl restart gdm`), or reboot.
  [Pen and tablet guide](docs/operations/pen-tablet-input.md).
- **Tablet support** (default pen mode): pressure, tilt and eraser over any
  network where the host supports pen input, with no host driver.
  [User guide: pen](docs/user-guide.md).
- **Presets:** Auto, Speed (60 fps), Grading (10-bit) and HDR, each its own
  pipeline. [Presets and pipelines](docs/features.md#streaming-presets-and-independent-pipelines).
- **Several displays:** the host can give each of your Mac's displays its own.
  [Features](docs/features.md#what-a-session-carries).
- **Audio and microphone**, **clipboard** both ways, and your Mac keyboard
  mapped the way the host expects. [User guide](docs/user-guide.md).

## Status (0.19.0)

| Platform | State |
| --- | --- |
| Linux Pier, X11 | Works: Auto, Speed, Grading; HDR served as Grading unless the desktop is declared PQ; Native tablet |
| Linux Pier, GNOME Wayland | Works where the installer's qualification proves it: Auto, Speed, Grading and HDR on GNOME 50+; Native tablet on native-seat desktops |
| Windows Pier | Works: Auto, Speed, Grading, HDR, several displays; Tablet support (Windows Ink); no Native tablet |
| macOS Pier | Preview: Auto, Speed, Grading, HDR with headroom; Native tablet with pressure for the Wacom Intuos5 touch L |
| macOS Deck | Works: Apple silicon and Intel apps (macOS 12+), signed and notarised; Native tablet on macOS 13+ (Intel untested with a real tablet) |
| Linux or Windows Deck | Does not exist yet: [help wanted](docs/project.md#where-help-is-wanted) |

What each host does in detail, and what was tested on real hardware, is in
[Features and status](docs/features.md#what-works-today). Changes are in the
[CHANGELOG](CHANGELOG.md).

## Learn more

- [User guide](docs/user-guide.md): installing, connecting, presets, colour
  check, pen, keyboard, displays, signing out.
- [Installing in detail](docs/install.md): every installer, the Linux desktop
  choice, uninstalling.
- [Features and status](docs/features.md): what works on each host, X11 and
  Wayland compared, keyboards, the presets.
- [Linux Pier on Wayland](docs/platforms/linux-wayland.md).
- [How Arcen works](docs/how-it-works.md): transport, Keel, security,
  architecture, writing a client.
- [Administration guide](docs/operations/pier-administration.md),
  [configuration reference](docs/operations/config-reference.md) and
  [error codes](docs/operations/error-codes.md).
- [Roadmap, help wanted, building from source](docs/project.md).

## Contributing and licence

Contributions are welcome: read [CONTRIBUTING.md](CONTRIBUTING.md) and accept
the [Contributor License Agreement](CLA.md) with a statement in your first pull
request. Arcen is licensed under the
[GNU AGPL 3.0 only](LICENSE) with the [Arcen Store Distribution Exception](LICENSE-EXCEPTION.md);
names and logos are covered by [TRADEMARKS.md](TRADEMARKS.md), and official
builds by [NOTICE](NOTICE) and [Licensing and official builds](docs/project.md#licensing-and-official-builds).
