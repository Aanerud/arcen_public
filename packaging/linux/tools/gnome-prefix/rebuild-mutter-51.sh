#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
# Lab only: rebuilds an existing prefix's Mutter 51.1 build tree incrementally after a lab
# patch (patch-pressure-curve-51.py) and installs it into a COPY of the prefix mounted at
# the prefix's own path, so the live prefix is never written by the container:
#   cp -a /opt/arcen-gnome/51 /opt/arcen-build/prefix51-copy
#   podman run --rm -v /opt/arcen-build/gnome-src:/src:Z \
#     -v /opt/arcen-build/prefix51-copy:/opt/arcen-gnome/51:Z \
#     localhost/arcen-gnome-build:el10 bash /src/rebuild-mutter-51.sh
# Then compare the copy with the live prefix (`diff -rq`) and swap only what changed.
set -euo pipefail
P=/opt/arcen-gnome/51
export PATH=$P/bin:/opt/venv/bin:/opt/cargo-tools/bin:$PATH
export PKG_CONFIG_PATH=$P/lib64/pkgconfig:$P/share/pkgconfig
export LDFLAGS="-Wl,-rpath,$P/lib64 -Wl,--disable-new-dtags"
export LD_LIBRARY_PATH=$P/lib64
export XDG_DATA_DIRS=$P/share:/usr/share
export GI_TYPELIB_PATH=$P/lib64/girepository-1.0:/usr/lib64/girepository-1.0
B=/src/b/mutter-51.1/_b
# As build-prefix.sh: rst2man and friends live in the venv only at build time.
/opt/venv/bin/pip -q install setuptools mako markdown docutils >/dev/null 2>&1 || true
ninja -C $B -j16 > /src/logs/mutter-rebuild.build.log 2>&1 || { tail -40 /src/logs/mutter-rebuild.build.log; exit 1; }
grep -E 'Compiling C object|Linking target' /src/logs/mutter-rebuild.build.log | sed 's/^\[[0-9/]*\] //' || true
ninja -C $B install > /src/logs/mutter-rebuild.install.log 2>&1 || { tail -40 /src/logs/mutter-rebuild.install.log; exit 1; }
sha256sum $P/lib64/libmutter-51.so.0.0.0
# The library a build with patch-pressure-curve-51.py produced: write-pressure-marker.sh
# vouches only for a library with this digest.
if grep -q 'return meta_bezier_lookup (evdev_tool->bezier, pressure);' \
  /src/b/mutter-51.1/src/backends/native/meta-input-device-tool-native.c; then
  sha256sum $P/lib64/libmutter-51.so.0.0.0 | cut -d' ' -f1 > /src/stamps/mutter-pressure-curve
fi
