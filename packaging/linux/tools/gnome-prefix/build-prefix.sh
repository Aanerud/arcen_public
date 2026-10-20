#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
# Lab only: builds the Arcen side-by-side GNOME 51 prefix inside the build container.
# Sources/build trees in /src (host /opt/arcen-build/gnome-src), output in $P (same path on host).
set -euo pipefail
P=${P:-/opt/arcen-gnome/51}
J=${J:-16}
S=/src; mkdir -p $S/dl $S/b $S/stamps $S/logs
export PATH=$P/bin:/opt/venv/bin:/opt/cargo-tools/bin:$PATH
export PKG_CONFIG_PATH=$P/lib64/pkgconfig:$P/share/pkgconfig
export LDFLAGS="-Wl,-rpath,$P/lib64 -Wl,--disable-new-dtags"
export LD_LIBRARY_PATH=$P/lib64
export XDG_DATA_DIRS=$P/share:/usr/share
export GI_TYPELIB_PATH=$P/lib64/girepository-1.0:/usr/lib64/girepository-1.0
export CARGO_HOME=$S/cargo-home
G=https://download.gnome.org/sources
/opt/venv/bin/pip -q install setuptools mako markdown docutils >/dev/null 2>&1 || true
fetch() { local url=$1 f=$S/dl/$(basename $1); [ -s $f ] || wget -q -O $f "$url"; echo $f; }
unpack() { local f=$1 d=$2; [ -d $S/b/$d ] || tar -C $S/b -xf $f; echo $S/b/$d; }
mesonb() { # name srcdir opts...
  local n=$1 d=$2; shift 2
  [ -f $S/stamps/$n ] && { echo "skip $n"; return; }
  echo "=== $n $(date)"
  rm -rf $d/_b
  meson setup $d/_b $d --prefix=$P --libdir=lib64 --buildtype=release -Db_lto=false "$@" > $S/logs/$n.setup.log 2>&1 || { tail -40 $S/logs/$n.setup.log; exit 1; }
  ninja -C $d/_b -j$J > $S/logs/$n.build.log 2>&1 || { tail -60 $S/logs/$n.build.log; exit 1; }
  ninja -C $d/_b install > $S/logs/$n.install.log 2>&1 || { tail -40 $S/logs/$n.install.log; exit 1; }
  touch $S/stamps/$n; echo "--- $n done $(date)"
}
# 1-3 glib + gobject-introspection (two-stage)
d=$(unpack $(fetch $G/glib/2.90/glib-2.90.1.tar.xz) glib-2.90.1)
mesonb glib-stage1 $d -Dintrospection=disabled -Dtests=false -Dman-pages=disabled -Ddocumentation=false -Dsysprof=disabled
d2=$(unpack $(fetch $G/gobject-introspection/1.86/gobject-introspection-1.86.0.tar.xz) gobject-introspection-1.86.0)
mesonb gobject-introspection $d2 -Dgtk_doc=false -Ddoctool=disabled -Dcairo=disabled
mesonb glib $d -Dintrospection=enabled -Dtests=false -Dman-pages=disabled -Ddocumentation=false -Dsysprof=disabled
# 4-5 wayland
d=$(unpack $(fetch https://gitlab.freedesktop.org/wayland/wayland/-/releases/1.26.0/downloads/wayland-1.26.0.tar.xz) wayland-1.26.0)
mesonb wayland $d -Ddocumentation=false -Dtests=false
d=$(unpack $(fetch https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/1.49/downloads/wayland-protocols-1.49.tar.xz) wayland-protocols-1.49)
mesonb wayland-protocols $d -Dtests=false
# 6 libinput
d=$(unpack $(fetch https://gitlab.freedesktop.org/libinput/libinput/-/archive/1.31.3/libinput-1.31.3.tar.gz) libinput-1.31.3)
mesonb libinput $d -Ddocumentation=false -Dtests=false -Ddebug-gui=false -Dlibwacom=true
# 7 libei main (1.7-dev)
if [ ! -d $S/b/libei ]; then git clone -q https://gitlab.freedesktop.org/libinput/libei.git $S/b/libei; fi
git -C $S/b/libei rev-parse HEAD > $S/b/libei.rev
mesonb libei $S/b/libei -Dtests=disabled -Ddocumentation=[]
# 8 pipewire (client libraries + modules; the Pier keeps the system daemon)
d=$(unpack $(fetch https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/1.6.9/pipewire-1.6.9.tar.gz) pipewire-1.6.9)
mesonb pipewire $d -Dauto_features=disabled -Dsession-managers=[] -Dexamples=disabled -Dtests=disabled -Dman=disabled -Ddocs=disabled \
  -Dspa-plugins=enabled -Dsupport=enabled -Daudioconvert=enabled -Dvideoconvert=enabled -Dcontrol=enabled -Ddbus=enabled -Dflatpak=disabled -Dpw-cat=disabled
# 9 schemas
d=$(unpack $(fetch $G/gsettings-desktop-schemas/51/gsettings-desktop-schemas-51.0.tar.xz) gsettings-desktop-schemas-51.0)
mesonb gsettings-desktop-schemas $d -Dintrospection=true
# 10 glycin (needs rustc >= 1.93; EL10 ships 1.92): a pinned rustup toolchain in /src
if [ ! -x $S/rustup/toolchains/1.96.1-x86_64-unknown-linux-gnu/bin/rustc ]; then
  wget -q -O $S/dl/rustup-init https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init
  chmod +x $S/dl/rustup-init
  RUSTUP_HOME=$S/rustup CARGO_HOME=$S/cargo-home $S/dl/rustup-init -y -q --no-modify-path --profile minimal --default-toolchain 1.96.1
fi
d=$(unpack $(fetch $G/glycin/2.2/glycin-2.2.3.tar.xz) glycin-2.2.3)
SYSPATH=$PATH; export PATH=$S/rustup/toolchains/1.96.1-x86_64-unknown-linux-gnu/bin:$PATH
mesonb glycin $d -Dglycin-loaders=true "-Dloaders=['glycin-image-rs']" -Dlibglycin=true -Dlibglycin-gtk4=false -Dintrospection=true -Dvapi=false -Dtests=false -Dglycin-thumbnailer=false
export PATH=$SYSPATH
# 11 mozjs-140
if [ ! -f $S/stamps/mozjs ]; then
  echo "=== mozjs $(date)"
  f=$(fetch https://archive.mozilla.org/pub/firefox/releases/140.17.0esr/source/firefox-140.17.0esr.source.tar.xz)
  [ -d $S/b/firefox-140.17.0 ] || tar -C $S/b -xf $f
  mkdir -p $S/b/firefox-140.17.0/js/src/_obj
  cd $S/b/firefox-140.17.0/js/src/_obj
  export M4=m4 AWK=awk AC_MACRODIR=$S/b/firefox-140.17.0/build/autoconf/
  # The distribution's rustc/cargo (1.92), not the rustup toolchain glycin used.
  unset CARGO_HOME RUSTUP_HOME; export CARGO_HOME=$S/cargo-mozjs
  CFLAGS="-O2" CXXFLAGS="-O2" ../configure --prefix=$P --libdir=$P/lib64 --with-system-zlib --disable-tests --disable-strip \
    --with-intl-api --enable-shared-js --enable-optimize --disable-debug --disable-jemalloc --disable-readline > $S/logs/mozjs.configure.log 2>&1 || { tail -40 $S/logs/mozjs.configure.log; exit 1; }
  make -j$J > $S/logs/mozjs.build.log 2>&1 || { tail -60 $S/logs/mozjs.build.log; exit 1; }
  make install > $S/logs/mozjs.install.log 2>&1 || { tail -40 $S/logs/mozjs.install.log; exit 1; }
  rm -f $P/lib64/libjs_static.ajs
  touch $S/stamps/mozjs; echo "--- mozjs done $(date)"; cd /
fi
# mozilla bug 1973994: the installed js-config.h must say XP_UNIX (Fedora carries the same fix)
grep -q "define XP_UNIX" $P/include/mozjs-140/js-config.h || \
  sed -i 's|#endif /\* js_config_h \*/|/* Arcen lab: mozilla bug 1973994 */\n#define XP_UNIX 1\n\n#endif /* js_config_h */|' $P/include/mozjs-140/js-config.h
# 12 gjs
d=$(unpack $(fetch $G/gjs/1.90/gjs-1.90.1.tar.xz) gjs-1.90.1)
mesonb gjs $d -Dprofiler=disabled -Dinstalled_tests=false -Dskip_dbus_tests=true -Dskip_gtk_tests=true -Dreadline=disabled
# 13 mutter + lab patches
d=$(unpack $(fetch $G/mutter/51/mutter-51.1.tar.xz) mutter-51.1)
if [ ! -f $S/stamps/mutter-patched ]; then
  for mr in 5358 5224; do fetch https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/$mr.patch >/dev/null; done
  (cd $d && git apply --verbose $S/dl/5358.patch > $S/logs/patch-5358.log 2>&1 || patch -p1 < $S/dl/5358.patch >> $S/logs/patch-5358.log 2>&1) || { cat $S/logs/patch-5358.log; exit 1; }
  (cd $d && git apply --verbose $S/dl/5224.patch > $S/logs/patch-5224.log 2>&1 || patch -p1 < $S/dl/5224.patch >> $S/logs/patch-5224.log 2>&1) || { cat $S/logs/patch-5224.log; exit 1; }
  python3 /src/patch-headless-seat-51.py $d/src/backends/native/meta-seat-impl.c > $S/logs/patch-seat.log 2>&1 || { cat $S/logs/patch-seat.log; exit 1; }
  touch $S/stamps/mutter-patched
fi
mesonb mutter $d -Dprofiler=false -Dtests=disabled -Dinstalled_tests=false -Dcogl_tests=false -Dclutter_tests=false -Dmutter_tests=false \
  -Ddocs=false -Dxwayland_path=/usr/bin/Xwayland -Dbash_completion=false -Ddevkit=disabled -Dcatch=false -Dkvm_tests=false -Dtty_tests=false
# 14 gnome-shell
d=$(unpack $(fetch $G/gnome-shell/51/gnome-shell-51.1.tar.xz) gnome-shell-51.1)
mesonb gnome-shell $d -Dextensions_tool=false -Dman=false -Dtests=false -Dportal_helper=false -Dcamera_monitor=false -Dgtk_doc=false
echo "=== ALL DONE $(date)"
