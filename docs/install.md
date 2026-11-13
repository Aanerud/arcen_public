# Installing Arcen in detail

Moved from the README (0.19.0). The quick start is in the
[README](../README.md#quick-start-try-it-in-10-minutes).

## Install

### The host (Pier)

Download the installer for your platform from
[Releases](https://github.com/Aanerud/arcen_public/releases) and run it with
administrative rights. One file. No runtime to install first, no package
repository to add, no service file to write.

**Linux** — needs systemd, `openssl`, and root. A download is not
executable, so set the bit first:

```sh
chmod +x install-arcen-pier-<version>-linux-x86_64
sudo ./install-arcen-pier-<version>-linux-x86_64
```

The Linux installer chooses the desktop from what the machine has, never from
the distribution's name, and prints what it found and why (also appended to
`/var/log/arcen/install.log`). An installed Xorg server keeps the dedicated
X11 desktop, exactly as before; an existing install never moves. Without
Xorg, a headless GNOME Shell 40+ with PipeWire, WirePlumber, `dbus-daemon`
and a DRM render node is qualified for Wayland with a short real session
(`arcen-pier wayland qualify --write`), and the service starts only if that
passes; otherwise the Pier stays installed but stopped, with what failed and
what to run next. A host with neither Xorg nor DRM is refused before
anything is written, naming the driver to install (for NVIDIA, `nvidia-drm`
with `modeset=1`). The one package the installer may install is `kernel-modules-internal` for
the running kernel, which carries the `vhci-hcd` USB/IP controller Native
tablet needs. It does so only when Native tablet is enabled (`--native-tablet`,
or `usb.native_tablet` already `"enabled"` on an upgrade), the host is in the
Rocky Linux/RHEL family, and the running kernel has no `vhci-hcd` module; it
installs the signed package with `dnf` and GPG checking on, and
`--no-native-tablet-modules` skips it. `--no-native-tablet` removes the
package again if the installer installed it. Apart from that it
installs no packages, and it never changes driver,
kernel, device-permission or boot configuration, or the SELinux mode; where
SELinux is enabled it loads only Arcen's own one-rule policy module (and
unloads it on uninstall), and where polkit is installed it adds Arcen's
rule that keeps remote desktops remote (see
[How Wayland support works](platforms/linux-wayland.md#how-wayland-support-works)). On a host
whose NVIDIA GPU has no display connectors, qualification also tries a
native desktop, which gives the pen pressure and tilt; the installer
explains the display-manager hazard that comes with it and disables the
display manager only with `--disable-gdm`. `--check` prints the detection
and decision and writes nothing.

**Windows** — open **PowerShell as Administrator**, then run the installer from
the folder you downloaded it to:

```powershell
.\install-arcen-pier.exe
```

**macOS** — a preview package, `ArcenPier-<version>.pkg`, signed and
notarised. Open it in Installer.app on the Mac it installs; the command-line
`installer` is refused, because the privacy approvals that follow need a
person at that Mac. It needs **macOS 14.2 or later** and serves the desktop of
a user who is signed in at the console. It is a preview, not yet a supported
host: the cold-boot login window and multi-display on physical hardware are
unqualified.

The macOS Pier is deliberately **more than one app**, and the installer says so
before it changes anything:

- **Arcen Pier**, the network service. It starts at boot, listens on UDP 18444,
  holds the host key and checks passwords. It cannot see or control any screen,
  and it runs as `_arcen`, a hidden account with no password and no shell. It
  refuses to run as root.
- **Arcen Agent Helper**, in `/Library/PrivilegedHelperTools`. macOS starts one
  in each signed-in session, as that person, to capture and to type. It has no
  port and cannot read the key.

They cannot be one program. macOS allows capture and input only inside a
signed-in session, while a host must answer when nobody is signed in and must
not fight over its port when two people are. Creating `_arcen` is why macOS asks
once, on first install, *"Installer would like to administer your computer"*.
Nothing else about any account changes. Each person who will be served then
approves Screen & System Audio Recording and Accessibility for Arcen Agent
Helper, and **System Audio Recording** (a separate list) when the helper asks:
the host keeps its own speakers silent during a session by default, and
refuses sessions until that approval is given. The full reasoning, what each part can and cannot do, and the one open
gap (the login-window agent runs as root) are in
[`docs/security/macos-pier-process-model.md`](security/macos-pier-process-model.md).

The installer creates the directories, generates a TLS certificate, registers
and starts the service, and opens **UDP 18444** on a firewall it recognises.
It succeeds only when the Pier is proven running. Every host gets the same
configuration with everything on — audio, microphone, clipboard, time zone,
10-bit video, multi-display — and the file exists for turning things off. An
upgrade keeps the existing configuration and the host's certificate, so
remembered Decks keep trusting it.

On Windows, **reboot afterwards**. Windows reads the list of credential
providers only when the login screen starts, so the provider Arcen just
installed stays invisible until the machine restarts. Skip the reboot and your
first remote sign-in fails, asking you to install something that is already
installed.

Useful flags on Linux and Windows: `--dry-run` shows what would happen and
changes nothing; `--uninstall` and `--uninstall --purge` are described under
[Uninstall](#uninstall).

### The client (Deck)

Download `Arcen-Deck-<version>-macOS.zip` for an Apple-silicon Mac, or
`Arcen-Deck-Intel-<version>-macOS.zip` for an Intel Mac, unzip it, and drag the
app (`Arcen Deck` or `Arcen Deck Intel`) to `/Applications`. Both are signed and
notarised, so they open normally, and both need macOS 12 or later. Install only
the one that matches your Mac: the two share one bundle identifier, so they
share settings and privacy permissions and are not meant to sit side by side.
Native tablet needs macOS 13 or later on the Deck; on Intel Macs it has not
been tested with a real tablet.

### Uninstall

Every host uninstaller keeps the host's identity by default — its TLS
certificate and configuration — so a reinstall presents the same certificate
and Decks that remember the host keep trusting it. Add `--purge` to remove that
too; it is a deliberate choice, because Decks must then pin the new
certificate.

**Linux Pier** — with the installer you installed from:

```sh
sudo ./install-arcen-pier-<version>-linux-x86_64 --uninstall          # keeps /etc/arcen and the logs
sudo ./install-arcen-pier-<version>-linux-x86_64 --uninstall --purge  # removes them as well
```

It stops and disables the service and removes the binary, the systemd unit,
the documentation and the log rotation rule, Arcen's SELinux policy module
where one was loaded, Arcen's polkit rule, the Wayland qualification
record and any leftover temporary qualification account, and the UDP 18444
firewall opening if the installer was the one that opened it. A display
manager that `--disable-gdm` turned off is put back. `--purge` first saves a copy of
`/etc/arcen/pier.json` as `/etc/arcen-pier.json.purged-<epoch>`.

**Windows Pier** — in **PowerShell as Administrator**, from the folder with the
installer:

```powershell
.\install-arcen-pier.exe --uninstall          # keeps C:\ProgramData\Arcen
.\install-arcen-pier.exe --uninstall --purge  # removes it as well
```

It removes the service, the binaries, the credential provider registration, the
event source and the installer's firewall rule. `--purge` first saves
`pier.json` as `C:\ProgramData\arcen-pier.json.purged-<epoch>`. If it reports
that the credential provider is still loaded by the sign-in screen, reboot and
confirm the directory is gone.

**macOS Pier** — the uninstaller ships inside the app, so it cannot go missing:

```sh
sudo "/Applications/Arcen Pier.app/Contents/Resources/uninstall.sh"          # keeps identity, config and logs
sudo "/Applications/Arcen Pier.app/Contents/Resources/uninstall.sh" --purge  # removes them as well
```

It stops the service and the helper in every signed-in session, removes the
launchd definitions, the PAM policy, the log rotation rule, the virtual
microphone and both applications, then checks that nothing is left running.
Without `--purge` it keeps `/Library/Application Support/Arcen` (identity and
configuration), `/Library/Logs/Arcen/Pier`, the hidden `_arcen` account and the
Screen Recording and Accessibility approvals; `--purge` removes all of them.

**Deck** — quit it and drag `Arcen Deck` (or `Arcen Deck Intel`) from
`/Applications` to the Bin. Saved hosts, pinned certificates and settings live in
`~/Library/Application Support/Arcen` (the Deck keeps nothing in the
keychain and never saves a password); delete that folder only
if you want the Deck to forget every host. If you used Native tablet, check
that the Arcen USB helper no longer appears under **System Settings → General →
Login Items & Extensions**.
