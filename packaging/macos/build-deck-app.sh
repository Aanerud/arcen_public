#!/usr/bin/env bash
# Build "Arcen Deck.app" — the macOS client bundle.
#
# Compiles arcen-deck-macos in release and assembles a minimal .app around it.
# Output: <repo>/Arcen Deck.app (git-ignored; regenerate any time with this script).
#
# Usage: packaging/macos/build-deck-app.sh [--no-build] [--release | --dev-sign]
#
# Three distinct, mutually exclusive assembly modes:
#   (default)    unsigned ordinary assembly; rejects any protected input.
#   --dev-sign   explicit local development signing: external Apple Development
#                identity/profile, verified team/bundle/entitlements, no
#                notarization. Never auto-discovers a keychain identity.
#   --release    explicit Developer ID release signing: external Developer ID
#                identity/profile, verified team/bundle/entitlements, CMS trust,
#                notarization, staple, and Gatekeeper assessment.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
APP="$REPO/Arcen Deck.app"
BIN="$REPO/target/release/arcen-deck"
PLIST="$HERE/Deck-Info.plist"
ENTITLEMENTS="${ARCEN_ENTITLEMENTS_FILE:-$HERE/Deck.entitlements}"
VALIDATOR="$HERE/validate_release_inputs.py"
CMS_VERIFIER_SOURCE="$HERE/verify-provisioning-cms.c"
NO_BUILD=0
RELEASE=0
DEV_SIGN=0
for argument in "$@"; do
  case "$argument" in
    --no-build) NO_BUILD=1 ;;
    --release) RELEASE=1 ;;
    --dev-sign) DEV_SIGN=1 ;;
    *) echo "error: unknown argument: $argument" >&2; exit 2 ;;
  esac
done
if [ "$RELEASE" -eq 1 ] && [ "$DEV_SIGN" -eq 1 ]; then
  echo "error: --release and --dev-sign are mutually exclusive" >&2
  exit 2
fi

if [ "$NO_BUILD" -eq 0 ]; then
  # Build with the toolchain pinned in rust-toolchain.toml, not with whatever
  # `cargo` happens to be first on PATH. On this machine Homebrew's `rust`
  # formula installs /opt/homebrew/bin/cargo, which shadows rustup's shims and
  # ignores rust-toolchain.toml entirely — so a bare `cargo build` here would
  # produce the Deck, the artefact users actually download, with an unpinned
  # compiler while the Linux and Windows Piers used the pinned one.
  CHANNEL="$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO/rust-toolchain.toml" | head -1)"
  [ -n "$CHANNEL" ] || { echo "error: no channel entry in $REPO/rust-toolchain.toml" >&2; exit 1; }
  command -v rustup >/dev/null 2>&1 || {
    echo "error: rustup is required to honour the pinned toolchain ($CHANNEL)." >&2
    echo "       A Homebrew-only Rust install cannot select it." >&2
    exit 1
  }
  CARGO=(rustup run "$CHANNEL" cargo)
  RESOLVED="$(rustup run "$CHANNEL" rustc --version 2>/dev/null || true)"
  case "$RESOLVED" in
    "rustc $CHANNEL "*) ;;
    *) echo "error: toolchain $CHANNEL resolved to '$RESOLVED'; expected rustc $CHANNEL" >&2; exit 1 ;;
  esac
  echo "==> building with $RESOLVED (pinned: $CHANNEL)"

  export ARCEN_BUILD_ID="${ARCEN_BUILD_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
  export ARCEN_SOURCE_REVISION="${ARCEN_SOURCE_REVISION:-$(git -C "$REPO" rev-parse --short=12 HEAD)}"
  export ARCEN_FEATURE_PROFILE="${ARCEN_FEATURE_PROFILE:-quic-default}"
  if [ "$RELEASE" -eq 1 ]; then
    export ARCEN_SIGNING_STATE="${ARCEN_SIGNING_STATE:-developer-id-release}"
  elif [ "$DEV_SIGN" -eq 1 ]; then
    export ARCEN_SIGNING_STATE="${ARCEN_SIGNING_STATE:-apple-development}"
  else
    export ARCEN_SIGNING_STATE="${ARCEN_SIGNING_STATE:-unsigned}"
  fi
  # Embedding the privileged helper and building the client that talks to it
  # are one decision, not two. Keeping them separate already shipped a bundle
  # whose UI reported "(not available in this build)" while the helper sat
  # right next to it.
  DECK_FEATURES=""
  if [ "${ARCEN_EMBED_USB_HELPER:-0}" = "1" ]; then
    DECK_FEATURES="--features usb-hard-lab"
    echo "==> cargo build --locked --release -p arcen-usb-helper"
    ( cd "$REPO" && "${CARGO[@]}" build --locked --release -p arcen-usb-helper )
  fi
  echo "==> cargo build --locked --release -p arcen-deck-macos $DECK_FEATURES"
  # shellcheck disable=SC2086
  ( cd "$REPO" && bash scripts/verify-opusic-source.sh && "${CARGO[@]}" build --locked --release -p arcen-deck-macos $DECK_FEATURES )
fi

[ -x "$BIN" ] || { echo "error: $BIN not found; build first"; exit 1; }
[ -f "$ENTITLEMENTS" ] || { echo "error: entitlements file not found: $ENTITLEMENTS"; exit 1; }
python3 "$REPO/scripts/verify_quic_product_binary.py" "$BIN"

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$PLIST" "$APP/Contents/Info.plist"
cp "$BIN" "$APP/Contents/MacOS/arcen-deck"
cp "$REPO/legal/THIRD_PARTY_NOTICES.md" "$APP/Contents/Resources/THIRD_PARTY_NOTICES.md"
# App icon. Generated from the first-party brand mark; regenerate with
# packaging/macos/build-app-icon.sh if AppIcon.svg changes.
[ -f "$HERE/AppIcon.icns" ] || { echo "error: app icon not found: $HERE/AppIcon.icns"; exit 1; }
cp "$HERE/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
chmod +x "$APP/Contents/MacOS/arcen-deck"

# Privileged USB helper. Only embedded when this build actually contains the
# Hard USB client, so an ordinary Light build ships no privileged payload at
# all. SMAppService requires the plist under Contents/Library/LaunchDaemons and
# honours BundleProgram relative to the bundle root.
# See docs/adr/0011-macos-privileged-usb-helper.md.
HELPER_BIN="$REPO/target/release/arcen-usb-helper"
HELPER_PLIST="$REPO/packaging/macos/tech.arcen.deck.usbhelper.plist"
EMBED_HELPER=0
if [ "${ARCEN_EMBED_USB_HELPER:-0}" = "1" ]; then
  [ -x "$HELPER_BIN" ] || { echo "error: ARCEN_EMBED_USB_HELPER=1 but $HELPER_BIN is missing; cargo build --release -p arcen-usb-helper"; exit 1; }
  [ -f "$HELPER_PLIST" ] || { echo "error: helper LaunchDaemon plist not found: $HELPER_PLIST"; exit 1; }
  mkdir -p "$APP/Contents/Library/LaunchDaemons"
  cp "$HELPER_BIN" "$APP/Contents/MacOS/arcen-usb-helper"
  chmod +x "$APP/Contents/MacOS/arcen-usb-helper"
  cp "$HELPER_PLIST" "$APP/Contents/Library/LaunchDaemons/tech.arcen.deck.usbhelper.plist"
  EMBED_HELPER=1
  echo "==> embedded privileged USB helper"
fi

if otool -L "$APP/Contents/MacOS/arcen-deck" | grep -Eiq '(^|[/[:space:]])libopus([.0-9-]*)?\.dylib'; then
    echo "error: Deck has a dynamic libopus dependency" >&2
    exit 1
fi
if find "$APP" \( -iname 'libopus*.dylib' -o -iname 'opus.framework' -o -iname 'libopus*.a' -o -iname 'opus.lib' \) -print -quit | grep -q .; then
    echo "error: Deck bundle contains a nested Opus payload" >&2
    exit 1
fi
if otool -L "$APP/Contents/MacOS/arcen-deck" | grep -Eiq '(^|[/[:space:]])(lib)?openh264([.0-9-]*)?\.dylib'; then
    echo "error: Deck has a dynamic OpenH264 dependency" >&2
    exit 1
fi
if find "$APP" \( -iname 'libopenh264*.dylib' -o -iname 'openh264.framework' -o -iname 'libopenh264*.a' -o -iname 'openh264.lib' \) -print -quit | grep -q .; then
    echo "error: Deck bundle contains a nested OpenH264 payload" >&2
    exit 1
fi

# Provisioning and signing identities are protected external inputs, whether
# for --dev-sign or --release. Only decoded nonsensitive metadata is
# validated; inputs are never discovered from the keychain, inventoried, or
# logged.
PROFILE="${ARCEN_PROVISIONING_PROFILE:-}"
SIGN_ID="${ARCEN_CODESIGN_IDENTITY:-}"
NOTARY_PROFILE="${ARCEN_NOTARY_KEYCHAIN_PROFILE:-}"
DEV_PROFILE="${ARCEN_DEV_PROVISIONING_PROFILE:-}"
DEV_SIGN_ID="${ARCEN_DEV_CODESIGN_IDENTITY:-}"

# Shared verify-then-sign path for both explicit signing modes. Trust the
# external profile's CMS/Apple signer chain before decoding or embedding it,
# validate its team/bundle/entitlements and profile class (release vs
# development — see PROFILE_CLASS below) against $ENTITLEMENTS, sign, then
# re-validate the resulting signature against the expected certificate class
# ("developer-id" or "apple-development") for $MODE ("release" or "dev").
run_signed_assembly() {
    local MODE="$1"
    local SIGNING_PROFILE="$2"
    local SIGNING_IDENTITY="$3"
    local IDENTITY_CLASS="$4"
    local PROFILE_CLASS="$5"

    command -v python3 >/dev/null || {
        echo "error: $MODE signing metadata validation requires python3" >&2
        exit 1
    }
    command -v xcrun >/dev/null || {
        echo "error: $MODE signing trust validation requires xcrun" >&2
        exit 1
    }
    xcrun --find clang >/dev/null || {
        echo "error: $MODE signing trust validation requires Apple clang" >&2
        exit 1
    }

    SIGN_TEMP="$(mktemp -d "${TMPDIR:-/tmp}/arcen-deck-$MODE.XXXXXX")"
    PROFILE_SNAPSHOT="$SIGN_TEMP/profile.provisionprofile"
    PROFILE_METADATA="$SIGN_TEMP/profile.plist"
    SIGNATURE_METADATA="$SIGN_TEMP/signature.txt"
    CMS_VERIFIER="$SIGN_TEMP/arcen-provisioning-cms-verifier"
    cleanup_signing_metadata() {
        rm -rf "$SIGN_TEMP"
    }
    trap cleanup_signing_metadata EXIT

    install -m 600 "$SIGNING_PROFILE" "$PROFILE_SNAPSHOT"
    xcrun clang -Os "$CMS_VERIFIER_SOURCE" \
        -framework Security \
        -framework CoreFoundation \
        -o "$CMS_VERIFIER"
    # `security cms` does not reliably propagate signer trust failures through
    # its exit status. Verify signature status, SecTrust, and exact reviewed Apple
    # provisioning signer, reviewed intermediate/root DER digests before decode.
    if ! "$CMS_VERIFIER" "$PROFILE_SNAPSHOT" >/dev/null 2>&1; then
        echo "error: external provisioning profile CMS signature or Apple trust chain is invalid" >&2
        exit 1
    fi
    if ! /usr/bin/security cms -D -i "$PROFILE_SNAPSHOT" -o "$PROFILE_METADATA" 2>/dev/null; then
        echo "error: external provisioning profile could not be decoded" >&2
        exit 1
    fi
    python3 "$VALIDATOR" \
        --profile "$PROFILE_METADATA" \
        --entitlements "$ENTITLEMENTS" \
        --profile-class "$PROFILE_CLASS"
    cp "$PROFILE_SNAPSHOT" "$APP/Contents/embedded.provisionprofile"
    echo "==> validated and embedded external provisioning profile"

    echo "==> codesign (external $MODE identity)"
    # Inside-out, never --deep: --deep applies one entitlement set uniformly to
    # every nested binary, which is wrong for a helper with a different role.
    if [ "$EMBED_HELPER" -eq 1 ]; then
      codesign --sign "$SIGNING_IDENTITY" \
               --options runtime \
               --timestamp \
               --force \
               "$APP/Contents/MacOS/arcen-usb-helper"
    fi
    codesign --sign "$SIGNING_IDENTITY" \
             --entitlements "$ENTITLEMENTS" \
             --options runtime \
             --force \
             "$APP"
    codesign --verify --deep --strict --verbose=2 "$APP"
    codesign --display --verbose=4 "$APP" > /dev/null 2>"$SIGNATURE_METADATA"
    python3 "$VALIDATOR" \
        --profile "$PROFILE_METADATA" \
        --entitlements "$ENTITLEMENTS" \
        --profile-class "$PROFILE_CLASS" \
        --signature "$SIGNATURE_METADATA" \
        --identity-class "$IDENTITY_CLASS"

    if [ "$MODE" = "release" ]; then
        echo "==> notarize"
        NOTARY_METADATA="$SIGN_TEMP/notary.json"
        ZIP="$SIGN_TEMP/Arcen-Deck-notarization.zip"
        ditto -c -k --keepParent "$APP" "$ZIP"
        xcrun notarytool submit "$ZIP" \
            --keychain-profile "$NOTARY_PROFILE" \
            --wait \
            --output-format json >"$NOTARY_METADATA"
        python3 -c 'import json,sys; data=json.load(open(sys.argv[1], encoding="utf-8")); sys.exit(0 if data.get("status") == "Accepted" else "notarization was not accepted")' "$NOTARY_METADATA"
        xcrun stapler staple "$APP" 2>&1
        xcrun stapler validate "$APP"
        spctl --assess --type execute --verbose=2 "$APP"
        echo "==> stapled — app is notarized and Gatekeeper-clean"
    else
        echo "==> signed with a local Apple Development identity — development mode only, not notarized"
    fi

    cleanup_signing_metadata
    trap - EXIT
}

if [ "$RELEASE" -eq 0 ] && [ "$DEV_SIGN" -eq 0 ]; then
    if [ -n "$PROFILE" ] || [ -n "$SIGN_ID" ] || [ -n "$NOTARY_PROFILE" ] || \
       [ -n "$DEV_PROFILE" ] || [ -n "$DEV_SIGN_ID" ]; then
        echo "error: protected signing inputs require explicit --dev-sign or --release mode" >&2
        exit 1
    fi
    echo "    UNSIGNED. Fine on this machine; Gatekeeper will reject it anywhere else."
    echo "    A copy sent to someone else opens as \"Arcen Deck.app is damaged and"
    echo "    can't be opened\", because it is quarantined and not notarized."
    echo "    For anything you hand to another person, build with --release:"
    echo "      ARCEN_PROVISIONING_PROFILE=<profile> \\"
    echo "      ARCEN_CODESIGN_IDENTITY=<Developer ID Application: ...> \\"
    echo "      ARCEN_NOTARY_KEYCHAIN_PROFILE=<notary profile> \\"
    echo "        packaging/macos/build-deck-app.sh --release"
elif [ "$DEV_SIGN" -eq 1 ]; then
    [ -n "$DEV_PROFILE" ] && [ -f "$DEV_PROFILE" ] || {
        echo "error: --dev-sign requires an external ARCEN_DEV_PROVISIONING_PROFILE file" >&2
        exit 1
    }
    [ -n "$DEV_SIGN_ID" ] || {
        echo "error: --dev-sign requires ARCEN_DEV_CODESIGN_IDENTITY" >&2
        exit 1
    }
    if [ -n "$PROFILE" ] || [ -n "$SIGN_ID" ] || [ -n "$NOTARY_PROFILE" ]; then
        echo "error: --dev-sign must not be combined with ARCEN_PROVISIONING_PROFILE, ARCEN_CODESIGN_IDENTITY, or ARCEN_NOTARY_KEYCHAIN_PROFILE" >&2
        exit 1
    fi
    run_signed_assembly "dev" "$DEV_PROFILE" "$DEV_SIGN_ID" "apple-development" "development"
else
    [ -n "$PROFILE" ] && [ -f "$PROFILE" ] || {
        echo "error: release requires an external ARCEN_PROVISIONING_PROFILE file" >&2
        exit 1
    }
    [ -n "$SIGN_ID" ] || {
        echo "error: release requires ARCEN_CODESIGN_IDENTITY" >&2
        exit 1
    }
    [ -n "$NOTARY_PROFILE" ] || {
        echo "error: release requires ARCEN_NOTARY_KEYCHAIN_PROFILE" >&2
        exit 1
    }
    if [ -n "$DEV_PROFILE" ] || [ -n "$DEV_SIGN_ID" ]; then
        echo "error: --release must not be combined with ARCEN_DEV_PROVISIONING_PROFILE or ARCEN_DEV_CODESIGN_IDENTITY" >&2
        exit 1
    fi
    run_signed_assembly "release" "$PROFILE" "$SIGN_ID" "developer-id" "release"
fi

echo "==> done: $APP"
echo "    double-click it, or: open \"$APP\""
