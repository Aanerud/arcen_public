#!/usr/bin/env bash
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "usb-vhci install must run as root" >&2
  exit 1
fi

readonly SOURCE_URL="https://git.code.sf.net/p/usb-vhci/vhci_hcd"
readonly SOURCE_COMMIT="79af411960bf229bd82797f0d42b654da8aae597"
readonly SOURCE_TREE="2b2657101bd42ad0f75cd59eec85c09dc997dfa1"
readonly PACKAGE_NAME="usb-vhci"
readonly PACKAGE_VERSION="1.15-arcen1"
readonly SOURCE_DIR="/usr/src/${PACKAGE_NAME}-${PACKAGE_VERSION}"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly KERNEL_VERSION="${1:-$(uname -r)}"
readonly KERNEL_BUILD="/lib/modules/${KERNEL_VERSION}/build"
installed=0

rollback_partial_install() {
  if [[ $installed -eq 0 ]]; then
    modprobe -r usb-vhci-iocifc 2>/dev/null || true
    modprobe -r usb-vhci-hcd 2>/dev/null || true
    rm -f /etc/modules-load.d/arcen-usb-vhci.conf
    dkms remove -m "$PACKAGE_NAME" -v "$PACKAGE_VERSION" --all 2>/dev/null || true
    rm -rf "$SOURCE_DIR"
  fi
}
trap rollback_partial_install ERR

for command in git patch dkms make modprobe; do
  command -v "$command" >/dev/null || {
    echo "missing required command: $command" >&2
    exit 1
  }
done
[[ -d "$KERNEL_BUILD" ]] || {
  echo "missing matching kernel-devel tree: $KERNEL_BUILD" >&2
  exit 1
}
[[ ! -e "$SOURCE_DIR" ]] || {
  echo "source directory already exists; uninstall first: $SOURCE_DIR" >&2
  exit 1
}

git clone --quiet "$SOURCE_URL" "$SOURCE_DIR"
git -C "$SOURCE_DIR" checkout --quiet --detach "$SOURCE_COMMIT"
[[ "$(git -C "$SOURCE_DIR" rev-parse HEAD)" == "$SOURCE_COMMIT" ]]
[[ "$(git -C "$SOURCE_DIR" rev-parse HEAD^{tree})" == "$SOURCE_TREE" ]]

for patch_file in \
  0001-rhel9-build-uaccess.patch \
  0002-rhel9-driver-attributes.patch \
  0003-rhel9-uaccess-class.patch
do
  patch --directory="$SOURCE_DIR" --strip=1 --forward \
    <"$SCRIPT_DIR/$patch_file"
done

install -D -m 0644 "$SCRIPT_DIR/usb-vhci.config.h" \
  "$SOURCE_DIR/conf/usb-vhci.config.h"

cat >"$SOURCE_DIR/dkms.conf" <<'EOF'
PACKAGE_NAME="usb-vhci"
PACKAGE_VERSION="1.15-arcen1"
MAKE[0]="make KERNELDIR=/lib/modules/${kernelver}/build"
CLEAN="make KERNELDIR=/lib/modules/${kernelver}/build clean"
BUILT_MODULE_NAME[0]="usb-vhci-hcd"
BUILT_MODULE_LOCATION[0]="."
DEST_MODULE_LOCATION[0]="/updates"
BUILT_MODULE_NAME[1]="usb-vhci-iocifc"
BUILT_MODULE_LOCATION[1]="."
DEST_MODULE_LOCATION[1]="/updates"
AUTOINSTALL="yes"
REMAKE_INITRD="no"
EOF

dkms add -m "$PACKAGE_NAME" -v "$PACKAGE_VERSION"
dkms build -m "$PACKAGE_NAME" -v "$PACKAGE_VERSION" -k "$KERNEL_VERSION"
dkms install -m "$PACKAGE_NAME" -v "$PACKAGE_VERSION" -k "$KERNEL_VERSION"

install -D -m 0644 /dev/stdin /etc/modules-load.d/arcen-usb-vhci.conf <<'EOF'
usb-vhci-hcd
usb-vhci-iocifc
EOF

modprobe usb-vhci-hcd
modprobe usb-vhci-iocifc
[[ -c /dev/usb-vhci ]] || {
  echo "modules loaded but /dev/usb-vhci was not created" >&2
  exit 1
}

installed=1
trap - ERR
echo "usb-vhci ${PACKAGE_VERSION} installed for ${KERNEL_VERSION}"
ls -l /dev/usb-vhci
