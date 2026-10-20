#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Writes the build part of PROVENANCE.md into a finished prefix: every source
# archive with its URL and SHA-256, the libei revision, every lab patch with
# its merge request and SHA-256, and the build container image.
set -euo pipefail
prefix=${1:?usage: write-provenance.sh <prefix> <src-dir>}
src=${2:?usage: write-provenance.sh <prefix> <src-dir>}
{
  echo "# Arcen side-by-side GNOME prefix: $prefix"
  echo
  echo "Lab build. Used only by Arcen's headless Wayland sessions"
  echo "(capture.wayland_compositor_prefix). Not a distribution package; not shipped."
  echo
  echo "- built: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "- build image: $(podman image inspect --format '{{.Id}}' localhost/arcen-gnome-build:el10 2>/dev/null || echo unknown) (FROM quay.io/rockylinux/rockylinux:10)"
  echo
  echo "## Sources"
  echo
  echo "| Archive | SHA-256 |"
  echo "| --- | --- |"
  for f in "$src"/dl/*.tar.*; do
    echo "| $(basename "$f") | $(sha256sum "$f" | cut -d' ' -f1) |"
  done
  echo
  echo "- libei: https://gitlab.freedesktop.org/libinput/libei.git @ $(cat "$src/b/libei.rev")"
  echo
  echo "## Lab patches (Mutter)"
  echo
  echo "| Patch | Source | SHA-256 | Upstream status at build |"
  echo "| --- | --- | --- | --- |"
  echo "| 5358.patch | https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/5358 | $(sha256sum "$src/dl/5358.patch" | cut -d' ' -f1) | merged on main 2026-10-14 (after 51.1) |"
  echo "| 5224.patch | https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/5224 | $(sha256sum "$src/dl/5224.patch" | cut -d' ' -f1) | open |"
  echo "| patch-headless-seat-51.py | Arcen lab: headless libinput only when ARCEN_LAB_HEADLESS_INPUT_SEAT names the udev seat | $(sha256sum "$src/patch-headless-seat-51.py" | cut -d' ' -f1) | not upstream |"
} > "$prefix/PROVENANCE.md"
echo "wrote $prefix/PROVENANCE.md"
