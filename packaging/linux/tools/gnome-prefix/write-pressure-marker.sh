#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
# Lab only: writes <prefix>/share/arcen/mutter-pressure-curve, which tells the
# Pier that the prefix's Mutter applies the tablet pressure curve once
# (patch-pressure-curve-51.py, GNOME/mutter#5077). The marker names Mutter's
# library and its SHA-256: the Pier stops inverting Mutter's squared pressure
# only while the library still has that digest, so restoring an unpatched
# library brings the inverse back by itself.
#
# Run on the host as root, after the patched library is installed:
#   write-pressure-marker.sh <prefix> [<src-dir>]
# <src-dir> (default /opt/arcen-build/gnome-src) must hold
# stamps/mutter-pressure-curve: the SHA-256 of the library that build-prefix.sh
# or rebuild-mutter-51.sh built with the patch and installed. The marker is
# written only when the prefix's library has that digest.
set -euo pipefail
prefix=${1:?usage: write-pressure-marker.sh <prefix> [<src-dir>]}
src=${2:-/opt/arcen-build/gnome-src}
built=$(head -c 64 "$src/stamps/mutter-pressure-curve" 2>/dev/null || true)
[[ $built =~ ^[0-9a-f]{64}$ ]] || {
  echo "$src/stamps/mutter-pressure-curve holds no digest: no Mutter was built with patch-pressure-curve-51.py" >&2
  exit 1
}
shopt -s nullglob
libraries=("$prefix"/lib64/libmutter-*.so.0)
[[ ${#libraries[@]} -eq 1 ]] || {
  echo "expected exactly one $prefix/lib64/libmutter-*.so.0, found ${#libraries[@]}" >&2
  exit 1
}
library=${libraries[0]#"$prefix"/}
digest=$(sha256sum "$(readlink -f "$prefix/$library")" | cut -d' ' -f1)
[[ $digest == "$built" ]] || {
  echo "$prefix/$library ($digest) is not the library the patched build produced ($built)" >&2
  exit 1
}
version=$("$prefix/bin/mutter" --version 2>/dev/null | head -1 | awk '{print $2}')
[[ $version =~ ^[0-9]+\.[0-9]+ ]] || {
  echo "$prefix/bin/mutter --version names no version" >&2
  exit 1
}
install -d -o root -g root -m 0755 "$prefix/share/arcen"
marker="$prefix/share/arcen/mutter-pressure-curve"
{
  echo "# Arcen lab: this prefix's Mutter applies the tablet pressure curve once"
  echo "# (GNOME/mutter#5077, packaging/linux/tools/gnome-prefix/patch-pressure-curve-51.py)."
  echo "# Written $(date -u +%Y-%m-%dT%H:%M:%SZ) by write-pressure-marker.sh."
  echo "arcen-mutter-pressure-curve=1"
  echo "mutter-version=$version"
  echo "library=$library"
  echo "library-sha256=$digest"
} > "$marker.new"
chown root:root "$marker.new"
chmod 0644 "$marker.new"
mv -f "$marker.new" "$marker"
command -v restorecon >/dev/null && restorecon -F "$prefix/share/arcen" "$marker" || true
cat "$marker"
