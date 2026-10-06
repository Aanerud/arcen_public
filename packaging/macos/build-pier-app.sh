#!/usr/bin/env bash
# Assembles the macOS Pier into a signed application bundle.
#
# Usage: packaging/macos/build-pier-app.sh [--identity "Developer ID Application: ..."]
#                                         [--notary-profile NAME]
#                                         [--with-driverkit-hid --provisioning-profile FILE]
#                                         [--with-virtual-hid --provisioning-profile FILE]
#
# --with-virtual-hid signs the Pier with com.apple.developer.hid.virtual.device,
# which lets it present a keyboard and pointer macOS treats as hardware — the
# only input that reaches the login window. The profile must be the Developer
# ID profile for pier.arcen.tech that authorizes it.
#
# The Pier is a background service, not something anyone double-clicks, so
# this looks like an odd thing to want. It is not optional.
#
# macOS grants Screen Recording, Accessibility and audio capture to a *bundle
# identity*, not to a path. A bare executable has no identity TCC can record,
# which has two consequences an operator meets immediately: the permission
# cannot be managed with `tccutil`, and a rebuilt or moved binary is a
# different subject that has to be approved again. Worse, a denial recorded
# against an unidentifiable subject cannot be reset, so the host is stuck
# refusing capture with no way back except approving it by hand.
#
# Wrapping the same executable in a bundle with a stable identifier fixes all
# of that, and costs one plist.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
BIN="$TARGET_DIR/release/arcen-pier-macos"
OUT="${ARCEN_MACOS_OUT:-$REPO/dist/macos}"
SCRATCH="${ARCEN_BUILD_SCRATCH:-$REPO/.arcen-build}/macos-app.$$"
mkdir -p "$SCRATCH"
APP="$OUT/Arcen Pier.app"
IDENTITY=""
NOTARY_PROFILE=""
PROVISIONING_PROFILE=""
WITH_DRIVERKIT_HID=0
WITH_VIRTUAL_HID=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --identity)
      shift
      IDENTITY="${1:-}"
      [[ -n "$IDENTITY" ]] || { echo "error: --identity needs a value" >&2; exit 2; }
      ;;
    --notary-profile)
      shift
      NOTARY_PROFILE="${1:-}"
      [[ -n "$NOTARY_PROFILE" ]] || { echo "error: --notary-profile needs a value" >&2; exit 2; }
      ;;
    --provisioning-profile)
      shift
      PROVISIONING_PROFILE="${1:-}"
      [[ -n "$PROVISIONING_PROFILE" ]] || { echo "error: --provisioning-profile needs a value" >&2; exit 2; }
      ;;
    --with-driverkit-hid)
      WITH_DRIVERKIT_HID=1
      ;;
    --with-virtual-hid)
      WITH_VIRTUAL_HID=1
      ;;
    *) echo "error: unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# Entitlements and the profile that authorizes them are a pair, and splitting
# them produces a binary the kernel kills with SIGKILL before main rather than
# an error anyone can read. Refuse here, where the message can say why, instead
# of shipping something that dies on a machine that has no profiles installed.
if (( WITH_DRIVERKIT_HID && WITH_VIRTUAL_HID )); then
  echo "error: choose one of --with-driverkit-hid and --with-virtual-hid" >&2
  exit 2
fi
WITH_PROFILE=$(( WITH_DRIVERKIT_HID || WITH_VIRTUAL_HID ))
if (( WITH_PROFILE )); then
  [[ -n "$PROVISIONING_PROFILE" ]] || {
    echo "error: --with-driverkit-hid and --with-virtual-hid require --provisioning-profile" >&2
    echo "       Signing those entitlements without an embedded profile yields a" >&2
    echo "       binary that AMFI kills (SIGKILL, no output) on any machine that" >&2
    echo "       does not already have the profile installed system-wide." >&2
    exit 2
  }
  [[ -n "$IDENTITY" ]] || {
    echo "error: --with-driverkit-hid requires --identity; entitlements are meaningless unsigned" >&2
    exit 2
  }
  [[ -f "$PROVISIONING_PROFILE" ]] || {
    echo "error: provisioning profile not found: $PROVISIONING_PROFILE" >&2
    exit 2
  }
fi
if [[ -n "$PROVISIONING_PROFILE" ]] && (( ! WITH_PROFILE )); then
  echo "error: --provisioning-profile is only meaningful with --with-driverkit-hid or --with-virtual-hid" >&2
  exit 2
fi

# Built here rather than merely checked for. Requiring the binary to exist
# already meant packaging whatever happened to be in target/release, which
# silently shipped a bundle older than the source it was built from — a fix
# was written, tested, packaged, installed, and then measured still failing,
# because the binary in the package predated it. The Deck script has always
# built its own binary; this one only looked.
echo "==> cargo build --locked --release -p arcen-pier-macos"
( cd "$REPO" && cargo build --locked --release -p arcen-pier-macos )

[[ -f "$BIN" ]] || {
  echo "error: $BIN is missing after a successful build" >&2
  exit 1
}

VERSION="$(awk -F'"' '/^version = /{print $2; exit}' "$REPO/Cargo.toml")"
[[ -n "$VERSION" ]] || { echo "error: could not read the workspace version" >&2; exit 1; }

rm -rf "$APP" "$OUT/Arcen Agent Helper.app"
mkdir -p "$APP/Contents/MacOS"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>pier.arcen.tech</string>
    <key>CFBundleName</key>
    <string>Arcen Pier</string>
    <key>CFBundleExecutable</key>
    <string>arcen-pier-macos</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <!-- A background agent, not an application. The host connects to the
         window server to read the cursor shape, which requires an
         NSApplication; without this it would acquire a Dock icon and a menu
         bar on a machine whose console belongs to whoever is sitting at it. -->
    <key>LSUIElement</key>
    <true/>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <!-- A host has no window and must not appear in the Dock or the switcher;
         it is a service that happens to need a bundle identity. -->
    <key>LSBackgroundOnly</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>14.2</string>
    <!-- Shown in the consent dialogs. An operator deciding whether to grant a
         permission deserves to read why it is wanted, in their own words
         rather than a framework default. -->
    <key>NSAudioCaptureUsageDescription</key>
    <string>Arcen sends this Mac's audio to the person connected to it, and silences the local speakers while it does.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>Arcen carries a connected user's microphone into applications running on this Mac.</string>
    <key>NSHumanReadableCopyright</key>
    <string>AGPL-3.0-only. Arcen comes with ABSOLUTELY NO WARRANTY.</string>
</dict>
</plist>
PLIST

cp "$BIN" "$APP/Contents/MacOS/arcen-pier-macos"
chmod 755 "$APP/Contents/MacOS/arcen-pier-macos"

# An icon is not decoration here. The consent dialog and the Privacy list both
# show it, and a blank placeholder beside a request to record the screen looks
# exactly like something an operator should refuse.
[[ -f "$HERE/AppIcon.icns" ]] || { echo "error: $HERE/AppIcon.icns is missing" >&2; exit 1; }
mkdir -p "$APP/Contents/Resources"
cp "$HERE/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"

# The agent helper: a separate bundle identity for the process that holds the
# permissions.
#
# This is not decoration. TCC records a grant against a bundle identity, so
# whichever identity asks for Screen Recording is the one an operator sees in
# Privacy settings and the one that keeps the grant. Putting that on the same
# identity as the network listener means the process terminating TLS and
# parsing untrusted input is also the process holding permanent authority to
# read the screen and synthesise input.
#
# Splitting them means the session helper carries the consent and the control
# plane does not. The reference implementation grants Screen Recording and
# Accessibility to its user agent rather than to one combined application,
# which is the same conclusion.
#
# The helper is a SIBLING bundle, not nested inside the Pier app. Nesting it
# was tried and TCC attributed the request to the container: the consent
# dialog said "Arcen Pier" and the Privacy list showed the Pier rather than
# the helper, which defeats the entire point of giving the helper its own
# identity. A top-level bundle has no container to be credited to.
#
# Today both roles are the same executable, so this bundle is the honest part
# of the split - the identity that consent attaches to - and the process
# separation follows. The helper is what the LaunchAgent runs.
HELPER="$OUT/Arcen Agent Helper.app"
mkdir -p "$HELPER/Contents/MacOS"
cat > "$HELPER/Contents/Info.plist" <<HPLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>pier.arcen.tech.agent</string>
    <key>CFBundleName</key>
    <string>Arcen Agent Helper</string>
    <key>CFBundleExecutable</key>
    <string>arcen-agent-helper</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <!-- A background agent, not an application. The host connects to the
         window server to read the cursor shape, which requires an
         NSApplication; without this it would acquire a Dock icon and a menu
         bar on a machine whose console belongs to whoever is sitting at it. -->
    <key>LSUIElement</key>
    <true/>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>LSBackgroundOnly</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>14.2</string>
    <key>NSAudioCaptureUsageDescription</key>
    <string>Arcen sends this Mac's audio to the person connected to it, and silences the local speakers while it does.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>Arcen carries a connected user's microphone into applications running on this Mac.</string>
    <key>NSHumanReadableCopyright</key>
    <string>AGPL-3.0-only. Arcen comes with ABSOLUTELY NO WARRANTY.</string>
</dict>
</plist>
HPLIST
cp "$BIN" "$HELPER/Contents/MacOS/arcen-agent-helper"
chmod 755 "$HELPER/Contents/MacOS/arcen-agent-helper"
mkdir -p "$HELPER/Contents/Resources"
cp "$HERE/AppIcon.icns" "$HELPER/Contents/Resources/AppIcon.icns"
plutil -lint "$HELPER/Contents/Info.plist" >/dev/null || {
  echo "error: the helper Info.plist is malformed" >&2
  exit 1
}

plutil -lint "$APP/Contents/Info.plist" >/dev/null || {
  echo "error: the generated Info.plist is malformed" >&2
  exit 1
}

# bash 3.2 is the newest bash macOS ships, and there `"${arr[@]}"` on an empty
# array is an unbound-variable error under `set -u`. Call sites below use the
# `${arr[@]+"${arr[@]}"}` form, which expands to nothing when the array is
# empty — which is what the default, entitlement-free build needs.
PIER_ENTITLEMENTS=()
if (( WITH_PROFILE )); then
  if (( WITH_VIRTUAL_HID )); then
    ENT="$HERE/Pier-virtual-hid.entitlements"
  else
    ENT="$HERE/Pier.entitlements"
  fi
  [[ -f "$ENT" ]] || { echo "error: $ENT is missing" >&2; exit 1; }

  SIGN_TEMP="$SCRATCH/sign"
  rm -rf "$SIGN_TEMP"
  mkdir -p "$SIGN_TEMP"
  trap 'rm -rf "$SIGN_TEMP"' EXIT
  PROFILE_SNAPSHOT="$SIGN_TEMP/profile.provisionprofile"
  PROFILE_METADATA="$SIGN_TEMP/profile.plist"
  CMS_VERIFIER="$SIGN_TEMP/arcen-provisioning-cms-verifier"

  install -m 600 "$PROVISIONING_PROFILE" "$PROFILE_SNAPSHOT"
  xcrun clang -Os "$HERE/verify-provisioning-cms.c" \
    -framework Security \
    -framework CoreFoundation \
    -o "$CMS_VERIFIER"
  # `security cms` does not reliably propagate signer trust failures through
  # its exit status, so trust is established before the decode rather than
  # inferred from it.
  if ! "$CMS_VERIFIER" "$PROFILE_SNAPSHOT" >/dev/null 2>&1; then
    echo "error: provisioning profile CMS signature or Apple trust chain is invalid" >&2
    exit 1
  fi
  if ! /usr/bin/security cms -D -i "$PROFILE_SNAPSHOT" -o "$PROFILE_METADATA" 2>/dev/null; then
    echo "error: provisioning profile could not be decoded" >&2
    exit 1
  fi
  # Checks the team, the expiry, the App ID, and that every entitlement asked
  # for is one this profile authorizes. A profile for the wrong product signs
  # without complaint and fails at launch.
  python3 "$HERE/validate_release_inputs.py" \
    --profile "$PROFILE_METADATA" \
    --entitlements "$ENT" \
    --bundle-id pier.arcen.tech \
    --profile-class release
  cp "$PROFILE_SNAPSHOT" "$APP/Contents/embedded.provisionprofile"
  PIER_ENTITLEMENTS=(--entitlements "$ENT")
  echo "==> validated and embedded provisioning profile ($(basename "$ENT"))"
fi

if [[ -n "$IDENTITY" ]]; then
  # Inside out: nested content first, then the executable, then the bundle
  # seals what is already signed. `--deep` is deliberately not used; it
  # re-signs nested content with the outer identity, which would give the
  # helper the Pier's identifier and undo the split this bundle exists for.
  codesign --force --options runtime --timestamp \
    --identifier pier.arcen.tech.agent \
    --sign "$IDENTITY" "$HELPER/Contents/MacOS/arcen-agent-helper"
  codesign --force --options runtime --timestamp \
    --identifier pier.arcen.tech.agent \
    --sign "$IDENTITY" "$HELPER"
  codesign --force --options runtime --timestamp \
    --identifier pier.arcen.tech ${PIER_ENTITLEMENTS[@]+"${PIER_ENTITLEMENTS[@]}"} \
    --sign "$IDENTITY" "$APP/Contents/MacOS/arcen-pier-macos"
  codesign --force --options runtime --timestamp \
    --identifier pier.arcen.tech ${PIER_ENTITLEMENTS[@]+"${PIER_ENTITLEMENTS[@]}"} \
    --sign "$IDENTITY" "$APP"
  codesign --verify --strict --verbose=2 "$APP" 2>&1 | sed 's/^/    /'
  echo "==> signed as pier.arcen.tech"
else
  echo "==> UNSIGNED. macOS will treat every rebuild as a new subject, so a"
  echo "    permission granted now is a permission you will grant again."
  echo "    Pass --identity \"Developer ID Application: ...\" for a stable identity."
fi

if [[ -n "$NOTARY_PROFILE" ]]; then
  [[ -n "$IDENTITY" ]] || {
    echo "error: notarization requires --identity; Apple will not notarize unsigned code" >&2
    exit 2
  }
  ZIP="$SCRATCH/ArcenPier.zip"
  # ditto, not zip: it preserves the bundle structure and extended attributes
  # notarization inspects.
  ditto -c -k --keepParent "$APP" "$ZIP"
  echo "==> notarizing (this waits for Apple)"
  xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait
  # Stapling attaches the ticket to the bundle, so a Mac with no network can
  # still verify it. Without this an offline install fails for a reason that
  # looks nothing like the cause.
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  spctl --assess --type exec -vv "$APP" 2>&1 | sed 's/^/    /'
  rm -rf "$(dirname "$ZIP")"
fi

echo "==> done: $APP"
echo "    run it with: \"$APP/Contents/MacOS/arcen-pier-macos\" serve [options]"
