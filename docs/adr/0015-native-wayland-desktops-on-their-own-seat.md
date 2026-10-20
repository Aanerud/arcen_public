# ADR 0015: Native Wayland desktops on their own logind seat

**Status:** Accepted (2026-10-21) for 0.17.0. Reviewed for the config and
qualification-record contract, and for security (udev rules, kernel module,
display manager option, polkit locality of seated sessions); verified on
EL10 / GNOME 49 / NVIDIA vGPU.

## Context

On stock GNOME a headless Wayland desktop has no pen. Headless Mutter opens
no input devices (`META_SEAT_NATIVE_FLAG_NO_LIBINPUT`), and Mutter's EIS
seat offers no `ei_stylus`. A Deck pen therefore arrives as a mouse with
pressure 0.5. The lab seat pen works, but only with a patched compositor
(mutter!5224 plus fixes).

Stock Mutter runs libinput in native mode. That mode needs a logind session
on a seat that holds a KMS device with a connector. The vGPUs Arcen hosts
run on have no connectors. The in-tree `vkms` driver provides one, and the
NVIDIA card node on the same seat renders.

Lab evidence (Rocky Linux 10.2, Mutter 49.4) is in
`docs/architecture/linux-wayland-provider.md`, section "Native desktops on
their own seat": with a real Deck GUI, Firefox and Chrome report a pen with
pressure and tilt, at the same latency as the patched-prefix lab seat pen.

## Decision

1. **Configuration contract.** `wayland.session_mode` (shared
   `arcen_session::pier_config::WaylandConfig`, the type
   `arcen_session::native_seat::SessionMode`) takes `auto` (default),
   `native_seat` or `headless`. Unknown tokens fail configuration loading.
   The lab keys `wayland.lab_seat_pen` and the patched-prefix route stay as
   they are and apply only to headless desktops.
2. **Qualification-record contract.** `QualifiedFacts.native_seat:
   Option<NativeSeatProof>` holds `tablet`, `pressure`, `tilt`,
   `gpu_id_path`, `display_manager_active` and `unavailable` (a
   `NativeSeatUnavailable` token).
   - Additive: absent means "not proven", and `SCHEMA_VERSION` stays 3.
   - An older Pier ignores the section; a newer Pier reads an older record
     as unproven.
   - The proof is a Wayland client of the qualification desktop's
     compositor. It must see the desktop's own tablet, and the pen tool with
     pressure and tilt once the tablet comes into proximity.
3. **Per-desktop plan** (`plan_desktop_seat`, `after_seat_failure`):
   - `auto` is native only with a proven record and a seat that can be
     built now; otherwise headless, with the reason logged.
   - `native_seat` refuses the desktop (`native_seat_unavailable`) when it
     cannot have its seat.
   - `headless` is never native.
4. **Host contract.** The Pier owns every host change, and all of them are
   volatile:
   - a rule in `/run/udev/rules.d/` for the NVIDIA card node, `vkms` and
     the desktop's tablet;
   - a `/run` override of `61-mutter.rules` without its `vkms` ignore
     lines;
   - `modprobe vkms`.

   The desktop's end, a Pier stop and the start sweep undo them all.
   Teardown removes the `vkms` card from the seat by dropping its current
   `seat` and `master-of-seat` tags, so it never joins `seat0`.
5. **Display manager.** The Pier never disables it. Each seat is built in an
   order in which GDM's login screen fails, and a seat holding any session
   is refused. The installer explains the remaining hazard (a GDM restart
   while a native desktop runs). It offers `--disable-gdm`, which records
   the state and is undone by `--restore-gdm` and `--uninstall`.
6. **polkit.** polkit counts a session with a seat as local, so a native
   desktop's remote user would get `allow_active` grants. Native desktops
   use the one mechanism every remote seated desktop uses (X11 on `seat0`
   too; ADR 0014, `arcen_session::polkit_remote`): there is no
   native-seat rule of its own.
   - `/etc/polkit-1/rules.d/05-arcen-remote-session.rules` (static;
     installed by the installer, rewritten at Pier start, removed by
     `--uninstall`) gives sessions with `REMOTE=1`, `TTY=arcen` or a
     `seat-arcen-pen-*` seat each action's `allow_any` answer, and
     `AUTH_ADMIN` for unknown actions. A `49-arcen-native-seat.rules` left
     by a development build of this ADR is removed.
   - Admission classifies a native desktop as `DesktopSeat::ArcenSeat`,
     never seatless: its logind session is on its Arcen seat. The launcher
     reads polkit's state before building the seat
     (`native_seat::polkit_blocker`): no rule, polkit < 0.106 or silent
     tools keep the desktop headless (`auto`) or refuse it (`native_seat`)
     before `vkms` is loaded. After launch the shared `pkcheck` proof
     (`polkit_remote::prove_desktop`, judged on the stricter of the planned
     seat and logind's record) must find the probe actions refused;
     otherwise the desktop is refused (`polkit_grants_remote_session`,
     `polkit_unverified`) and, until the Pier restarts, its later desktops
     are headless (`auto`) or refused (`native_seat`).
   - Qualification's `native_seat` step records `polkit_effective` only for
     the shared admission's `Admit` on an Arcen seat; a proof without it
     proves nothing.
   - polkit < 0.106 has no JavaScript rules: native mode is unavailable
     (`polkit_no_rules`) and desktops are headless.
7. **Security.** No ACL is granted on input devices. logind brokers them
   (`TakeDevice` for the session's own seat only) and revokes them
   (`EVIOCREVOKE`).
8. **Protocol.** Unchanged. The hello's pen truth already distinguishes a
   real tablet (`linux_uinput`) from the emulated seat.

## Consequences

- **One native desktop per host:** a card node belongs to one seat, and
  kernel 6.12 has one `vkms` device.
- **A seat held until the next connection:** a disconnected native desktop
  holds the seat until the next connection reaps it (the idle timeout is
  lazy, as for every desktop).
- **Records need re-qualification:** existing records stay headless until
  the host is re-qualified (automatic re-qualification does it on upgrade).
- **Upstream simplifications:**
  - Mutter: native mode with zero outputs when virtual monitors are
    requested, which removes `vkms` and the rules override.
  - GDM: a per-seat ignore, which removes the display-manager hazard.
