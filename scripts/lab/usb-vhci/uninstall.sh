#!/usr/bin/env bash
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "usb-vhci uninstall must run as root" >&2
  exit 1
fi

readonly PACKAGE_NAME="usb-vhci"
readonly PACKAGE_VERSION="1.15-arcen1"

modprobe -r usb-vhci-iocifc 2>/dev/null || true
modprobe -r usb-vhci-hcd 2>/dev/null || true
rm -f /etc/modules-load.d/arcen-usb-vhci.conf
dkms remove -m "$PACKAGE_NAME" -v "$PACKAGE_VERSION" --all 2>/dev/null || true
rm -rf /usr/src/usb-vhci-1.15-arcen1

echo "usb-vhci ${PACKAGE_VERSION} removed"
