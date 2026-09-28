#!/usr/bin/env bash
# Builds the macOS Pier installer package.
#
# Usage:
#   packaging/macos/build-pier-pkg.sh [--identity "Developer ID Application: ..."]
#                                     [--installer-identity "Developer ID Installer: ..."]
#                                     [--with-virtual-hid --provisioning-profile FILE]
#
# Produces `dist/macos/ArcenPier-<version>.pkg`, meant to be opened with
# Installer.app on the Mac it installs. The command-line `installer` is refused
# unless ARCEN_ALLOW_COMMAND_LINE_INSTALL=1 is set; see pier/distribution.xml.
#
# What it installs
# ----------------
# Two processes, the same split the Linux and Windows Piers use:
#
# * the network service, a LaunchDaemon running as the unprivileged `_arcen`
#   account from boot. It owns UDP 18444, the TLS key and admission, and has
#   no desktop. It is why the host stays reachable when nobody is logged in
#   and when users switch.
# * the desktop agent, a LaunchAgent that launchd starts in every graphical
#   session as that session's user. It captures, injects input, reads the
#   pasteboard and taps audio — the things macOS only allows inside a session
#   and only grants to a bundle identity — and holds no key and no port.
#
# The service hands each admitted Deck to the agent of the session on the
# console over a local socket and relays the stream.
#
# The install scripts live in packaging/macos/pier/ as ordinary files so they
# can be read, linted and tested; nothing here is generated from a heredoc.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
OUT="$REPO/dist/macos"
APP="$OUT/Arcen Pier.app"
IDENTITY=""
INSTALLER_IDENTITY=""
NOTARY_PROFILE=""
PROVISIONING_PROFILE=""
WITH_VIRTUAL_HID=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --identity) shift; IDENTITY="${1:-}" ;;
    --installer-identity) shift; INSTALLER_IDENTITY="${1:-}" ;;
    --notary-profile) shift; NOTARY_PROFILE="${1:-}" ;;
    --provisioning-profile) shift; PROVISIONING_PROFILE="${1:-}" ;;
    --with-virtual-hid) WITH_VIRTUAL_HID=1 ;;
    *) echo "error: unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

VERSION="$(awk -F'"' '/^version = /{print $2; exit}' "$REPO/Cargo.toml")"
[[ -n "$VERSION" ]] || { echo "error: could not read the workspace version" >&2; exit 1; }

echo "==> building the Pier application bundle"
APP_ARGS=()
[[ -n "$IDENTITY" ]] && APP_ARGS+=(--identity "$IDENTITY")
# Not passed down: the applications are notarized below, after this script
# has added their resources and re-sealed them. A ticket for the bundle the
# app script produced would not match the one that ships.
(( WITH_VIRTUAL_HID )) && APP_ARGS+=(--with-virtual-hid)
[[ -n "$PROVISIONING_PROFILE" ]] && APP_ARGS+=(--provisioning-profile "$PROVISIONING_PROFILE")
# bash 3.2 is the newest bash macOS ships, and there `"${arr[@]}"` on an empty
# array is an unbound-variable error under `set -u`. An unsigned build passes
# no arguments, so the default invocation is exactly the case that fails.
"$HERE/build-pier-app.sh" ${APP_ARGS[@]+"${APP_ARGS[@]}"}
[[ -d "$APP" ]] || { echo "error: $APP was not produced" >&2; exit 1; }

ROOT="$(mktemp -d)"
SCRIPTS="$(mktemp -d)"
trap 'rm -rf "$ROOT" "$SCRIPTS"' EXIT
PIER_SCRIPTS="$HERE/pier"
for file in common.sh preinstall postinstall uninstall.sh newsyslog.conf \
            distribution.xml welcome.html conclusion.html; do
  [[ -f "$PIER_SCRIPTS/$file" ]] || { echo "error: missing $PIER_SCRIPTS/$file" >&2; exit 1; }
done
for file in common.sh preinstall postinstall uninstall.sh; do
  bash -n "$PIER_SCRIPTS/$file" || { echo "error: $file does not parse" >&2; exit 1; }
done

mkdir -p "$ROOT/Applications" "$ROOT/Library/PrivilegedHelperTools" \
  "$ROOT/Library/LaunchAgents" "$ROOT/Library/LaunchDaemons" \
  "$ROOT/private/etc/pam.d" "$ROOT/private/etc/newsyslog.d"
# COPYFILE_DISABLE stops macOS writing AppleDouble "._" sidecars for extended
# attributes. Without it the package installs a shadow file beside every real
# one, which is visible to anyone who looks and is the kind of detail that
# makes an installer look unfinished.
COPYFILE_DISABLE=1 cp -R "$APP" "$ROOT/Applications/"
# The helper is its own bundle, not nested in the Pier, so TCC credits the
# helper's identity instead of walking up to a container. It is a background
# component, so it goes with the other privileged helpers, not /Applications.
COPYFILE_DISABLE=1 cp -R "$OUT/Arcen Agent Helper.app" "$ROOT/Library/PrivilegedHelperTools/"
cp "$HERE/pam/arcen" "$ROOT/private/etc/pam.d/arcen"
chmod 644 "$ROOT/private/etc/pam.d/arcen"
cp "$PIER_SCRIPTS/newsyslog.conf" "$ROOT/private/etc/newsyslog.d/pier.arcen.tech.conf"
chmod 644 "$ROOT/private/etc/newsyslog.d/pier.arcen.tech.conf"

# The launchd definitions come from the binary being packaged, rendered by the
# same code its tests check. A copy typed into this script is how a label, a
# path or an argument drifts from what the program expects.
PIER_BIN="$APP/Contents/MacOS/arcen-pier-macos"
"$PIER_BIN" launchd-plist daemon > "$ROOT/Library/LaunchDaemons/pier.arcen.tech.service.plist"
"$PIER_BIN" launchd-plist agent > "$ROOT/Library/LaunchAgents/pier.arcen.tech.agent.plist"
for plist in "$ROOT/Library/LaunchDaemons/pier.arcen.tech.service.plist" \
             "$ROOT/Library/LaunchAgents/pier.arcen.tech.agent.plist"; do
  plutil -lint "$plist" >/dev/null || { echo "error: $plist is malformed" >&2; exit 1; }
  chmod 644 "$plist"
done

cp "$PIER_SCRIPTS/common.sh" "$PIER_SCRIPTS/preinstall" "$PIER_SCRIPTS/postinstall" "$SCRIPTS/"
chmod 755 "$SCRIPTS/preinstall" "$SCRIPTS/postinstall"
chmod 644 "$SCRIPTS/common.sh"

# One source of truth for the shipped configuration: the template file, copied
# into the bundle, which postinstall installs only when none exists.
CONFIG_TEMPLATE="$HERE/arcen-pier.json"
[[ -f "$CONFIG_TEMPLATE" ]] || { echo "missing configuration template: $CONFIG_TEMPLATE" >&2; exit 1; }
RESOURCES="$ROOT/Applications/Arcen Pier.app/Contents/Resources"
mkdir -p "$RESOURCES"
python3 -m json.tool "$CONFIG_TEMPLATE" >/dev/null || { echo "error: $CONFIG_TEMPLATE is not JSON" >&2; exit 1; }
cp "$CONFIG_TEMPLATE" "$RESOURCES/pier.json"
chmod 644 "$RESOURCES/pier.json"

# The uninstaller ships inside the bundle so it cannot go missing.
cp "$PIER_SCRIPTS/uninstall.sh" "$PIER_SCRIPTS/common.sh" "$RESOURCES/"
chmod 755 "$RESOURCES/uninstall.sh"
chmod 644 "$RESOURCES/common.sh"

# Strip extended attributes before sealing. `pkgbuild` encodes any xattr it
# finds as an AppleDouble entry in the payload, so a tree carrying quarantine
# or Finder metadata produces a "._" companion for every file. Clearing them
# first is what removes the companions; deleting the companions afterwards
# does not, because they are generated at packaging time rather than copied.
#
# This runs before signing, because the seal has to cover the tree as it will
# actually ship.
xattr -cr "$ROOT" 2>/dev/null || true

# Re-sign the bundle: adding the uninstaller after signing broke the seal.
if [[ -n "$IDENTITY" ]]; then
  codesign --force --options runtime --timestamp --identifier pier.arcen.tech.agent \
    --sign "$IDENTITY" "$ROOT/Library/PrivilegedHelperTools/Arcen Agent Helper.app"
  # The Pier's entitlements and embedded profile were set by the app build.
  # Re-sealing without preserving them strips the entitlement, and the
  # profile left behind no longer matches the signature.
  codesign --force --options runtime --timestamp --identifier pier.arcen.tech \
    --preserve-metadata=entitlements \
    --sign "$IDENTITY" "$ROOT/Applications/Arcen Pier.app"
fi

# Notarize the applications exactly as they will ship. Stapled, so a Mac with
# no route to Apple can still verify them. This is not ceremony: macOS honours
# a restricted entitlement such as virtual HID only on notarized Developer ID
# code, and refuses it with nothing more than "failed MACF" otherwise.
if [[ -n "$NOTARY_PROFILE" ]]; then
  [[ -n "$IDENTITY" ]] || { echo "error: notarization requires --identity" >&2; exit 2; }
  NOTARY_DIR="$(mktemp -d)"
  NOTARY_ZIP="$NOTARY_DIR/ArcenPierApps.zip"
  # Both bundles in one submission, wherever the payload puts them.
  mkdir "$NOTARY_DIR/apps"
  ditto "$ROOT/Applications/Arcen Pier.app" "$NOTARY_DIR/apps/Arcen Pier.app"
  ditto "$ROOT/Library/PrivilegedHelperTools/Arcen Agent Helper.app" \
    "$NOTARY_DIR/apps/Arcen Agent Helper.app"
  ditto -c -k --keepParent "$NOTARY_DIR/apps" "$NOTARY_ZIP"
  echo "==> notarizing the applications (this waits for Apple)"
  xcrun notarytool submit "$NOTARY_ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
  for app in "$ROOT/Applications/Arcen Pier.app" "$ROOT/Library/PrivilegedHelperTools/Arcen Agent Helper.app"; do
    xcrun stapler staple "$app"
    spctl --assess --type exec -vv "$app" 2>&1 | sed 's/^/    /'
  done
  rm -rf "$NOTARY_DIR"
fi

mkdir -p "$OUT"
PKG="$OUT/ArcenPier-$VERSION.pkg"
rm -f "$PKG"

find "$ROOT" -name '.DS_Store' -delete

# Bundles are relocatable by default: Installer looks for an existing copy
# with the same identifier anywhere on the disk and "upgrades" that instead.
# A renamed backup such as "Arcen Pier.app.prev" is exactly such a copy, and a
# package that updates it leaves /Applications stale while reporting success.
COMPONENTS="$SCRIPTS.components.plist"
pkgbuild --analyze --root "$ROOT" "$COMPONENTS" >/dev/null
index=0
while plutil -extract "$index" xml1 -o /dev/null "$COMPONENTS" >/dev/null 2>&1; do
  plutil -replace "$index.BundleIsRelocatable" -bool NO "$COMPONENTS"
  index=$((index + 1))
done
(( index > 0 )) || { echo "error: pkgbuild found no bundles to pin" >&2; exit 1; }
trap 'rm -rf "$ROOT" "$SCRIPTS" "$COMPONENTS"' EXIT

echo "==> building $PKG"
PRODUCT="$(mktemp -d)"
trap 'rm -rf "$ROOT" "$SCRIPTS" "$COMPONENTS" "$PRODUCT"' EXIT
COMPONENT_PKG="pier.arcen.tech.pkg"
pkgbuild \
  --root "$ROOT" \
  --component-plist "$COMPONENTS" \
  --scripts "$SCRIPTS" \
  --identifier pier.arcen.tech \
  --version "$VERSION" \
  --install-location / \
  --ownership recommended \
  "$PRODUCT/$COMPONENT_PKG" >/dev/null

# The component is wrapped in a product archive, which is what Installer.app
# opens. It exists for its installation check: the package is meant to be
# installed on the Mac itself, by the person who approves its privacy
# permissions, and the check refuses the command-line `installer` unless the
# administrator asks for it explicitly (see pier/distribution.xml). A check in
# preinstall cannot do this. Measured on the lab Mac: the scripts do not see
# the caller's environment, and a failing script only tells Terminal that the
# install failed, not why.
#
# The architectures and minimum macOS version come from what was just built,
# so the installer refuses a Mac the binaries cannot run on before it installs
# anything.
ARCHITECTURES="$(lipo -archs "$APP/Contents/MacOS/arcen-pier-macos" | tr ' ' ',')"
MINIMUM="$(plutil -extract LSMinimumSystemVersion raw "$APP/Contents/Info.plist")"
[[ -n "$ARCHITECTURES" && -n "$MINIMUM" ]] || {
  echo "error: could not read the Pier's architectures or minimum macOS version" >&2
  exit 1
}
mkdir -p "$PRODUCT/resources"
cp "$PIER_SCRIPTS/welcome.html" "$PIER_SCRIPTS/conclusion.html" "$PRODUCT/resources/"
sed -e "s/@VERSION@/$VERSION/g" \
    -e "s/@COMPONENT@/$COMPONENT_PKG/g" \
    -e "s/@HOST_ARCHITECTURES@/$ARCHITECTURES/g" \
    -e "s/@MINIMUM_SYSTEM_VERSION@/$MINIMUM/g" \
    "$PIER_SCRIPTS/distribution.xml" > "$PRODUCT/distribution.xml"
if grep -q '@[A-Z_]*@' "$PRODUCT/distribution.xml"; then
  echo "error: distribution.xml still has an unfilled placeholder" >&2
  exit 1
fi
xmllint --noout "$PRODUCT/distribution.xml" || { echo "error: distribution.xml is malformed" >&2; exit 1; }
productbuild \
  --distribution "$PRODUCT/distribution.xml" \
  --resources "$PRODUCT/resources" \
  --package-path "$PRODUCT" \
  "$PKG.unsigned" >/dev/null

if [[ -n "$INSTALLER_IDENTITY" ]]; then
  productsign --sign "$INSTALLER_IDENTITY" "$PKG.unsigned" "$PKG"
  rm -f "$PKG.unsigned"
  echo "==> signed with $INSTALLER_IDENTITY"
else
  mv "$PKG.unsigned" "$PKG"
  echo "==> UNSIGNED PACKAGE."
  echo "    Distributing this needs a 'Developer ID Installer' certificate,"
  echo "    which is a different certificate from 'Developer ID Application'."
  echo "    Without it Gatekeeper refuses the package on any other Mac."
fi

# The package is notarized separately from the application inside it. Both
# are needed: Gatekeeper checks the package when it is opened, and the app
# when it is launched. Notarizing only the payload leaves a package that is
# refused before its contents are ever examined.
if [[ -n "$NOTARY_PROFILE" ]]; then
  if [[ -z "$INSTALLER_IDENTITY" ]]; then
    echo "==> skipping package notarization: Apple will not notarize an unsigned package" >&2
  else
    echo "==> notarizing the package"
    xcrun notarytool submit "$PKG" --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$PKG"
    spctl --assess --type install -vv "$PKG" 2>&1 | sed 's/^/    /'
  fi
fi

echo "==> done: $PKG"
pkgutil --check-signature "$PKG" 2>&1 | head -3 || true
