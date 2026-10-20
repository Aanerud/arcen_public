#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
# Lab only: turns a rebuilt libmutter-17 (build-patched-mutter-49.sh) into the
# prefix /opt/arcen-gnome/49: the distribution's GNOME Shell 49 with that one
# library. Usage: make-prefix-49.sh <dir holding libmutter-17.so*>
set -euo pipefail
src=${1:?usage: make-prefix-49.sh <dir holding libmutter-17.so*>}
P=/opt/arcen-gnome/49
install -d -m 755 $P/bin $P/lib64 $P/share
cp -a "$src"/libmutter-17.so* $P/lib64/
cat > $P/bin/gnome-shell <<'SH'
#!/bin/sh
# Arcen lab: the distribution GNOME Shell 49 with a lab-patched libmutter-17. See PROVENANCE.md.
LD_LIBRARY_PATH=/opt/arcen-gnome/49/lib64:/usr/lib64/mutter-17${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export LD_LIBRARY_PATH
exec /usr/bin/gnome-shell "$@"
SH
chmod 755 $P/bin/gnome-shell
chown -R root:root $P
{
  echo "# Arcen lab prefix: $P"
  echo
  echo "- bin/gnome-shell: wrapper running the distribution's /usr/bin/gnome-shell ($(rpm -q gnome-shell))"
  echo "- lib64/libmutter-17.so.0.0.0: $(rpm -q mutter) SRPM + patch-headless-seat-49.py, sha256 $(sha256sum $P/lib64/libmutter-17.so.0.0.0 | cut -d' ' -f1)"
} > $P/PROVENANCE.md
semanage fcontext -a -e /usr $P 2>/dev/null || true
restorecon -RF $P
$P/bin/gnome-shell --version
