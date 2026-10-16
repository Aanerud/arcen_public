#!/usr/bin/env bash
# Verify a governed, locally patched third-party source tree against its
# ARCEN_SOURCE_MANIFEST.sha256: every listed file unchanged, nothing added.
#
# usage: scripts/verify-vendored-source.sh <directory under third_party/>
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 <third_party directory>" >&2; exit 2; }
name="${1%/}"
name="${name#third_party/}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_root="$repo/third_party/$name"
[ -d "$source_root" ] || { echo "error: no such vendored source: $source_root" >&2; exit 1; }
manifest="$source_root/ARCEN_SOURCE_MANIFEST.sha256"

[ -f "$manifest" ] || { echo "error: missing governed source manifest: $manifest" >&2; exit 1; }

expected_paths="$(mktemp)"
actual_paths="$(mktemp)"
trap 'rm -f "$expected_paths" "$actual_paths"' EXIT

while IFS= read -r line; do
  hash="${line%%  *}"
  path="${line#*  }"
  if [[ ! "$hash" =~ ^[0-9a-f]{64}$ ]] || [ "$path" = "$line" ] || [ -z "$path" ]; then
    echo "error: malformed source manifest line: $line" >&2
    exit 1
  fi
  printf '%s\n' "$path" >>"$expected_paths"
  if command -v sha256sum >/dev/null 2>&1; then
    actual_hash="$(sha256sum "$source_root/$path" | cut -d' ' -f1)"
  else
    actual_hash="$(shasum -a 256 "$source_root/$path" | cut -d' ' -f1)"
  fi
  if [ "$actual_hash" != "$hash" ]; then
    echo "error: changed governed source file: $path" >&2
    exit 1
  fi
done <"$manifest"

# Only regular files and directories may exist. A symlink is invisible to the
# file inventory below but is still followed by the crates' build scripts (the
# apple-cf Swift collector compiles `.swift` symlinks and descends into linked
# directories), so it could smuggle ungoverned source into a verified build.
unsupported="$(cd "$source_root" && find . ! -type f ! -type d -print | sed 's#^\./##' | LC_ALL=C sort)"
if [ -n "$unsupported" ]; then
  echo "error: governed source tree contains entries that are not regular files or directories:" >&2
  printf '%s\n' "$unsupported" >&2
  exit 1
fi

(cd "$source_root" && find . -type f ! -name ARCEN_SOURCE_MANIFEST.sha256 -print |
  sed 's#^\./##' | LC_ALL=C sort) >"$actual_paths"
LC_ALL=C sort -o "$expected_paths" "$expected_paths"
if ! cmp -s "$expected_paths" "$actual_paths"; then
  echo "error: governed source paths do not match the manifest" >&2
  diff -u "$expected_paths" "$actual_paths" >&2 || true
  exit 1
fi

echo "Verified $(wc -l <"$actual_paths" | tr -d ' ') governed $name source files."
