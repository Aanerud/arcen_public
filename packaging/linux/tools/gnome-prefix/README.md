# Side-by-side GNOME prefix (lab tool)

Builds a newer GNOME Shell and Mutter into a prefix such as
`/opt/arcen-gnome/51`, used only by Arcen's headless Wayland sessions
(`capture.wayland_compositor_prefix`). The why and the mechanism are in
[`docs/architecture/linux-wayland-provider.md`](../../../../docs/architecture/linux-wayland-provider.md#running-a-newer-gnome-than-the-distribution-ships).
The operator procedure, including the rollback, is in
[`docs/operations/pier-administration.md`](../../../../docs/operations/pier-administration.md#upgrading-the-gnome-session-on-el10).

| File | Runs | Does |
| --- | --- | --- |
| `Containerfile` | `podman build` | Rocky Linux 10 build image with every build dependency. The host never gets a `-devel` package. |
| `build-prefix.sh` | in the container | Downloads pinned sources and builds them in order into the prefix. Resumable through stamps in `/src/stamps`. |
| `patch-headless-seat-51.py` | in the container | Arcen lab edit on top of mutter!5224, described below. |
| `patch-pressure-curve-51.py` | in the container (or on the build tree) | Arcen lab fix for GNOME/mutter#5077, described below. |
| `rebuild-mutter-51.sh` | in the container, with a *copy* of the prefix mounted at the prefix's path | Rebuilds an existing prefix's Mutter after a lab patch, without the container writing the live prefix. |
| `write-pressure-marker.sh` | on the host (root) | Writes `share/arcen/mutter-pressure-curve`: the patched library's path and SHA-256, so the Pier stops inverting Mutter's squared pressure while that library is installed. |
| `finish-prefix.sh` | on the host (root) | Links the system typelibs and GIO modules the prefix lacks, refreshes caches and schemas, and checks that every library resolves. |
| `build-patched-mutter-49.sh`, `patch-headless-seat-49.py` | in a `rockylinux:10` container (`-e NVR=mutter-<version>.el10_2`, `-v <dir>:/lab:Z`) | Rebuilds the host's own Mutter 49 SRPM with the headless-input lab edit; copies out `libmutter-17.so*` only |
| `make-prefix-49.sh` | on the host (root) | Makes `/opt/arcen-gnome/49`: that library plus a `bin/gnome-shell` wrapper running the distribution's GNOME Shell 49. This is the smallest prefix that gives `wayland.lab_seat_pen` a real pen on EL10's own GNOME. |
| `write-provenance.sh` | on the host (root) | Writes `PROVENANCE.md` into the prefix: archives and their SHA-256, the libei revision, and each lab patch with its merge request and digest. |

The edit made by `patch-headless-seat-51.py`:

- A headless compositor runs libinput only when
  `ARCEN_LAB_HEADLESS_INPUT_SEAT` names a udev seat.
- Devices it opens directly are made non-blocking and close-on-exec.

The fix made by `patch-pressure-curve-51.py` (GNOME/mutter#5077, open with no
merge request as of 2026-11-15; Mutter `main` unchanged):

- The tablet pressure a client sees is the pressure curve's value. Since 46
  (e7ba16689) Mutter multiplied it by the pressure again, so the default
  linear curve squared it (0.5 arrived as 0.245).
- The curve's 256-sample lookup interpolates by the position's fraction
  between two samples (it used the position itself), with the last sample at
  1.0. A linear curve then maps every pressure to itself.

On an existing prefix: back up the two source files, run the script on the
build tree, `rebuild-mutter-51.sh` into a copy, check with `diff -rq` that only
`lib64/libmutter-51.so.0.0.0` changed, keep the live library, swap the new one
in (`install` to a new name, then `mv`) while no Arcen desktop runs, `touch
<src>/stamps/mutter-pressure-curve` and run `write-pressure-marker.sh`. The Pier
reads the marker each time it builds a desktop's seat pen and inverts only
libinput's threshold while the library's digest matches; restoring the old
library (or removing the marker) brings the inverse back by itself. A local
tablet and a Native tablet (USB passthrough) need no Pier help: their
pressure reaches applications as libinput reports it.

This is a lab tool. It is not part of any release artefact, and nothing it
downloads is redistributed. A prefix built with the lab patches (mutter!5358,
mutter!5224 plus the seat edit, and the #5077 fix) is for evaluation; shipping waits for those
changes to be released upstream.
