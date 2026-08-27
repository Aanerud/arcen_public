#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_root="$repo/third_party/opusic-sys-0.7.3-arcen1"
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

(cd "$source_root" && find . -type f ! -name ARCEN_SOURCE_MANIFEST.sha256 -print |
  sed 's#^\./##' | LC_ALL=C sort) >"$actual_paths"
LC_ALL=C sort -o "$expected_paths" "$expected_paths"
if ! cmp -s "$expected_paths" "$actual_paths"; then
  echo "error: governed source paths do not match the manifest" >&2
  diff -u "$expected_paths" "$actual_paths" >&2 || true
  exit 1
fi

echo "Verified $(wc -l <"$actual_paths" | tr -d ' ') governed opusic-sys source files."
