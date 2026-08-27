#!/usr/bin/env bash
# Regenerate packaging/macos/AppIcon.icns from AppIcon.svg.
#
# The .icns is committed so an ordinary Deck build needs no SVG toolchain, but
# it is a generated artefact. Run this whenever AppIcon.svg changes.
#
# AppIcon.svg embeds the first-party brand mark from the Arcen site
# (site/assets/arcen-mark.svg) on the brand graphite ground. The ground is not
# decoration: the mark's deck line is #f2efe8 and its anchors are #8f8d86, so on
# a light background those strokes disappear and only the copper arch survives.
#
# Requires: rsvg-convert (librsvg) and iconutil (macOS).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SVG="$HERE/AppIcon.svg"
OUT="$HERE/AppIcon.icns"

command -v rsvg-convert >/dev/null 2>&1 || {
    echo "error: rsvg-convert not found (brew install librsvg)" >&2
    exit 1
}
command -v iconutil >/dev/null 2>&1 || {
    echo "error: iconutil not found; this script requires macOS" >&2
    exit 1
}
[ -f "$SVG" ] || { echo "error: $SVG not found" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
SET="$WORK/AppIcon.iconset"
mkdir -p "$SET"

# Render each size from the vector rather than downscaling one raster, so the
# hairline strokes stay crisp at 16px.
for size in 16 32 64 128 256 512 1024; do
    rsvg-convert -w "$size" -h "$size" "$SVG" -o "$SET/icon_${size}x${size}.png"
done

# iconutil's expected @2x names.
mv "$SET/icon_32x32.png"     "$SET/icon_32x32.png.tmp"
cp "$SET/icon_32x32.png.tmp" "$SET/icon_16x16@2x.png"
mv "$SET/icon_32x32.png.tmp" "$SET/icon_32x32.png"
cp "$SET/icon_64x64.png"     "$SET/icon_32x32@2x.png"
cp "$SET/icon_256x256.png"   "$SET/icon_128x128@2x.png"
cp "$SET/icon_512x512.png"   "$SET/icon_256x256@2x.png"
cp "$SET/icon_1024x1024.png" "$SET/icon_512x512@2x.png"
rm -f "$SET/icon_64x64.png" "$SET/icon_1024x1024.png"

iconutil -c icns "$SET" -o "$OUT"
echo "wrote $OUT"
