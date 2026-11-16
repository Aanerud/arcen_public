#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
# Regenerate the Windows Deck icon images from the macOS Deck's AppIcon.svg.
#
# The PNGs are committed so the Windows build needs no SVG toolchain; build.ps1
# packs them into the .ico it links into arcen-deck.exe and uses for the
# Start-menu shortcut and Apps & features. Same artwork as the Mac Deck (1:1
# look and feel). Run this whenever packaging/macos/AppIcon.svg changes.
#
# Requires: rsvg-convert (librsvg).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SVG="$HERE/../../macos/AppIcon.svg"
command -v rsvg-convert >/dev/null 2>&1 || {
    echo "error: rsvg-convert not found (brew install librsvg)" >&2
    exit 1
}
[ -f "$SVG" ] || { echo "error: $SVG not found" >&2; exit 1; }
# The sizes Windows asks an .ico for at 100-200% scaling, plus 256 for
# Explorer's large views. Each is rendered from the vector so 16 px stays crisp.
for size in 16 20 24 32 40 48 64 256; do
    rsvg-convert -w "$size" -h "$size" "$SVG" -o "$HERE/ArcenDeck-$size.png"
done
echo "wrote $HERE/ArcenDeck-{16,20,24,32,40,48,64,256}.png"
