#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
# Lab only (inside a throwaway rockylinux:10 container): rebuild the host's exact
# mutter SRPM with the headless-libinput lab patch; copy only libmutter-17 out.
set -euxo pipefail
NVR=${NVR:?}
dnf -y -q install dnf-plugins-core rpm-build python3 >/dev/null
dnf config-manager --set-enabled crb devel appstream-source
mkdir -p /lab/out /lab/prefix/lib64
cd /lab
[ -f src/$NVR.src.rpm ] || (mkdir -p src && cd src && dnf -q download --source $NVR)
dnf -y -q builddep src/$NVR.src.rpm >/dev/null
T=/lab/rpmbuild
rm -rf $T; mkdir -p $T
rpm -i --define "_topdir $T" src/$NVR.src.rpm
rpmbuild -bp --nodeps --define "_topdir $T" $T/SPECS/mutter.spec >/lab/out/bp.log 2>&1
SRC=$(dirname $(find $T/BUILD -path '*src/backends/native/meta-seat-impl.c' | head -1))/../../..
SRC=$(readlink -f $SRC)
F=$SRC/src/backends/native/meta-seat-impl.c
cp $F /lab/out/meta-seat-impl.c.orig
python3 /lab/patch-headless-seat-49.py $F
diff -u /lab/out/meta-seat-impl.c.orig $F > /lab/out/lab-headless-libinput.diff || true
rpmbuild -bc --short-circuit --nocheck --nodeps --define "_topdir $T" $T/SPECS/mutter.spec >/lab/out/bc.log 2>&1
mkdir -p /lab/prefix/lib64
cp -a $(find $T/BUILD -path '*/src/*' -name 'libmutter-17.so*' ! -name '*.p' ! -name '*.symbols') /lab/prefix/lib64/
ls -l /lab/prefix/lib64
