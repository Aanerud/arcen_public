# Side-by-side GNOME prefix (lab tool)

Builds a newer GNOME Shell and Mutter into a prefix such as
`/opt/arcen-gnome/51`, used only by Arcen's headless Wayland sessions
(`capture.wayland_compositor_prefix`). The why and the mechanism are in
[`docs/architecture/linux-wayland-provider.md`](../../../../docs/architecture/linux-wayland-provider.md#running-a-newer-gnome-than-the-distribution-ships-wp-gnomeupgrade).
The operator procedure, including the rollback, is in
[`docs/operations/pier-administration.md`](../../../../docs/operations/pier-administration.md#upgrading-the-gnome-session-on-el10).

| File | Runs | Does |
| --- | --- | --- |
| `Containerfile` | `podman build` | Rocky Linux 10 build image with every build dependency. The host never gets a `-devel` package. |
| `build-prefix.sh` | in the container | Downloads pinned sources and builds them in order into the prefix. Resumable through stamps in `/src/stamps`. |
| `patch-headless-seat-51.py` | in the container | Arcen lab edit on top of mutter!5224, described below. |
| `finish-prefix.sh` | on the host (root) | Links the system typelibs and GIO modules the prefix lacks, refreshes caches and schemas, and checks that every library resolves. |
| `build-patched-mutter-49.sh`, `patch-headless-seat-49.py` | in a `rockylinux:10` container (`-e NVR=mutter-<version>.el10_2`, `-v <dir>:/lab:Z`) | Rebuilds the host's own Mutter 49 SRPM with the headless-input lab edit; copies out `libmutter-17.so*` only |
| `make-prefix-49.sh` | on the host (root) | Makes `/opt/arcen-gnome/49`: that library plus a `bin/gnome-shell` wrapper running the distribution's GNOME Shell 49. This is the smallest prefix that gives `wayland.lab_seat_pen` a real pen on EL10's own GNOME. |
| `write-provenance.sh` | on the host (root) | Writes `PROVENANCE.md` into the prefix: archives and their SHA-256, the libei revision, and each lab patch with its merge request and digest. |

The edit made by `patch-headless-seat-51.py`:

- A headless compositor runs libinput only when
  `ARCEN_LAB_HEADLESS_INPUT_SEAT` names a udev seat.
- Devices it opens directly are made non-blocking and close-on-exec.

This is a lab tool. It is not part of any release artefact, and nothing it
downloads is redistributed. A prefix built with the lab patches (mutter!5358,
mutter!5224 plus the seat edit) is for evaluation; shipping waits for those
changes to be released upstream.
