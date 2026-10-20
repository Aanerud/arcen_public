#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Finishes a side-by-side GNOME prefix on the host it will run on (as root):
# links the system typelibs and GIO modules the prefix does not build, so the
# prefix's newer GLib finds them, refreshes the GIO module cache and the
# prefix's schemas, and writes the runtime part of PROVENANCE.md.
# Idempotent; re-run after a system update that adds typelibs or GIO modules.
set -euo pipefail

prefix=${1:?usage: finish-prefix.sh <prefix>}
[[ -x "$prefix/bin/gnome-shell" || -x "$prefix/bin/mutter" ]] || {
  echo "$prefix holds no bin/gnome-shell or bin/mutter" >&2
  exit 1
}

link_missing() { # system_dir prefix_dir pattern
  local from=$1 to=$2 pattern=$3 linked=0
  mkdir -p "$to"
  for file in "$from"/$pattern; do
    [[ -e "$file" ]] || continue
    local name
    name=$(basename "$file")
    if [[ ! -e "$to/$name" || -L "$to/$name" ]]; then
      ln -sfn "$file" "$to/$name"
      linked=$((linked + 1))
    fi
  done
  echo "linked $linked from $from"
}

link_missing /usr/lib64/girepository-1.0 "$prefix/lib64/girepository-1.0" '*.typelib'
link_missing /usr/lib64/gio/modules "$prefix/lib64/gio/modules" '*.so'
"$prefix/bin/gio-querymodules" "$prefix/lib64/gio/modules"
"$prefix/bin/glib-compile-schemas" "$prefix/share/glib-2.0/schemas"

missing=$(for binary in "$prefix"/bin/gnome-shell "$prefix"/bin/mutter "$prefix"/lib64/*.so*; do
  [[ -e "$binary" ]] && ldd "$binary" 2>/dev/null | grep 'not found' || true
done | sort -u)
if [[ -n "$missing" ]]; then
  echo "missing runtime libraries (install the runtime package, never -devel):" >&2
  echo "$missing" >&2
  exit 2
fi

{
  echo
  echo "## Finished on the host"
  echo
  echo "- date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "- os: $(. /etc/os-release && echo "$ID $VERSION_ID")"
  echo "- gnome-shell: $("$prefix/bin/gnome-shell" --version 2>/dev/null || echo absent)"
  echo "- mutter: $("$prefix/bin/mutter" --version 2>/dev/null | head -1 || echo absent)"
} >> "$prefix/PROVENANCE.md"
echo "finished $prefix"
