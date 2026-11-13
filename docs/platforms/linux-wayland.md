# Linux Pier on Wayland

Moved from the README (0.19.0).

## How Wayland support works

**The Pier picks the desktop from facts, then proves it.** At start
(`capture.provider`, default `auto`) an installed Xorg server means X11.
Without one, Wayland needs a GNOME Shell/Mutter that can run headless with
virtual monitors, systemd-logind, `dbus-daemon`, PipeWire, WirePlumber, a
DRM render node — and a qualification record. `arcen-pier wayland qualify
--write`, which the installer runs, signs a temporary account into a real
desktop and measures what this host can actually do: whether GPU import of
the compositor's DMA-BUFs is pixel-exact, which display sizes hold Speed's
rate, whether the compositor's cursor metadata, Xwayland and the precision
needed for Grading and HDR are there, and whether a native desktop works.
The service re-qualifies when the Pier, GNOME, PipeWire or the GPU driver
changes. A capability the record does not prove is never assumed: the
session is served at the level that was proven and the Deck shows why. The
lab hosts' evidence is in
[`docs/architecture/wayland-qualification/`](../architecture/wayland-qualification).

**Each sign-in gets a whole GNOME session of its own** — a logind session, a
private D-Bus bus, PipeWire and WirePlumber, then GNOME Shell with the
keyring unlocked by the sign-in password, the settings daemons and the
user's autostart programs. Arcen drives Mutter's own ScreenCast and
RemoteDesktop D-Bus APIs on that private bus: video arrives as PipeWire
buffers in the same capenc that serves X11, input goes in through
RemoteDesktop and its EIS socket, and so does the clipboard. There is no
portal and no screen-sharing prompt, and the compositor is the
distribution's own (or, for Grading and HDR where the distribution ships
GNOME 49 or older, an optional newer GNOME installed beside it).

**Two ways to run that session** (`wayland.session_mode`):

- **Headless** — GNOME Shell with `--headless` and virtual monitors, in a
  seatless session. It needs no seat and changes nothing on the host. Its
  limit is the pen: headless Mutter turns libinput off
  ([`META_SEAT_NATIVE_FLAG_NO_LIBINPUT`](https://gitlab.gnome.org/GNOME/mutter/-/raw/main/src/backends/native/meta-backend-native.c)),
  so it cannot see a tablet, and the remote-input route meant for one — a
  stylus over libei — is not there yet: libei's `ei_stylus` is merged
  ([libei!400](https://gitlab.freedesktop.org/libinput/libei/-/merge_requests/400),
  [announcement](https://who-t.blogspot.com/2026/07/libei-and-graphics-tablets-stylus.html))
  but not in a release, and Mutter's EIS server offers no stylus. The Pier
  probes for one at every sign-in and reports what it found; until a
  compositor offers one, the pen acts as a mouse. (Enabling libinput in headless
  Mutter, [mutter!5224](https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/5224),
  is still open; Arcen keeps a lab-only route for it, off by default.)
- **Native** — the same stock compositor, started as a display server on a
  logind seat of the desktop's own (`seat-arcen-pen-<id>`). Native Mutter
  runs libinput, so the Deck's pen reaches applications as a real tablet
  (`zwp_tablet_v2`) with pressure and tilt. Native mode needs a display head
  with a connector, which a vGPU does not have, so the seat holds three
  devices: the NVIDIA card node, which renders; the kernel's own virtual KMS
  driver, `vkms`, as the head; and the desktop's virtual tablet. The Pier
  puts them there with volatile udev rules under `/run`, logind hands them to
  the compositor and revokes them when the session ends, and everything is
  undone at the desktop's end, at Pier stop and at the next start. One
  native desktop runs per host; the default, `auto`, uses it when
  qualification proved it and otherwise serves a headless desktop and logs
  why. A display-manager restart would put a login screen on that seat, so
  the installer explains it and offers `--disable-gdm`.

**A seat makes polkit treat a session as local**, ignoring that logind
recorded it as remote. That is true of every X11 desktop too, and before
0.17.0 it gave remote X11 users the console's rights. The installer now
writes one rule, `/etc/polkit-1/rules.d/05-arcen-remote-session.rules`,
that gives remote, Arcen-launched and Arcen-seat sessions polkit's remote
answers, and the Pier asks polkit (`pkcheck`) about each seated desktop
before handing it over, refusing it on any grant.

Details: [the Wayland provider](../architecture/linux-wayland-provider.md)
(how each part works, with lab evidence),
[the administration guide](../operations/pier-administration.md#which-desktop-a-linux-pier-serves)
(choosing, qualifying, native desktops, polkit, the optional GNOME prefix),
[ADR 0014](../adr/0014-quic-lanes-and-capture-contracts.md) (capture
contracts) and
[ADR 0015](../adr/0015-native-wayland-desktops-on-their-own-seat.md)
(native desktops).
