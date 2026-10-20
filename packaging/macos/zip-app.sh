#!/usr/bin/env bash
# Zips an application bundle for distribution without AppleDouble ("._")
# entries, and refuses a zip that has any.
#
# Usage: packaging/macos/zip-app.sh APP ZIP
#
# A plain `ditto -c -k` stores each extended attribute as a "._" companion
# inside the zip, and on macOS 26 every file the build wrote carries
# `com.apple.provenance`, which cannot be removed. A signed bundle keeps its
# signature, seal and stapled ticket in ordinary files, so dropping the
# attributes, resource forks and quarantine changes nothing that verifies.
set -euo pipefail

[[ $# -eq 2 ]] || { echo "usage: $0 APP ZIP" >&2; exit 2; }
APP="$1"
ZIP="$2"
[[ -d "$APP" ]] || { echo "error: $APP is not a bundle directory" >&2; exit 1; }

rm -f "$ZIP"
ditto -c -k --norsrc --noextattr --noqtn --keepParent "$APP" "$ZIP"
if zipinfo -1 "$ZIP" | grep -E '(^|/)\._|^__MACOSX/' >/dev/null; then
  echo "error: $ZIP has AppleDouble (._) entries" >&2
  exit 1
fi
