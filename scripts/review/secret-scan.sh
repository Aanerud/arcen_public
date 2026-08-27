#!/usr/bin/env bash
# secret-scan.sh: refuse any commit that carries an operational secret.
#
# Companion to doc-guard.sh. Where doc-guard protects documentation content,
# this protects against the opposite failure: content that should never have
# been written down at all.
#
# The patterns come from the environment, never from a file in the repository,
# because a file of secret patterns is itself a file of secrets.
#
# Usage
#   export ARCEN_SECRET_VALUES=$'...\n...$'   # private, per-machine list
#   scripts/review/secret-scan.sh [path ...]     # default: the whole worktree
#
# Exit status
#   0  no secret found
#   1  a secret was found; the file and line are named, the value never is
#   2  usage or environment error
set -uo pipefail

if [ -z "${ARCEN_SECRET_VALUES:-}" ]; then
  echo "secret-scan: ARCEN_SECRET_VALUES is empty." >&2
  echo "  Set ARCEN_SECRET_VALUES to a newline-separated list of literal values." >&2
  echo "  Refusing to pass vacuously: an unarmed scanner is worse than none." >&2
  exit 2
fi

ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$ROOT" || exit 2

# macOS ships bash 3.2, which has no `mapfile`. Using it silently produced an
# EMPTY target list and a vacuous pass, which is worse than no scanner at all.
# Build the list portably instead, and refuse to run on an empty list.
FILELIST=$(mktemp)
trap 'rm -f "$FILELIST"' EXIT

if [ "$#" -gt 0 ]; then
  printf '%s\n' "$@" > "$FILELIST"
else
  # Everything git knows about, plus untracked files. Build artifacts are
  # excluded by .gitignore and are not committed.
  git ls-files --cached --others --exclude-standard > "$FILELIST" 2>/dev/null
fi

TARGET_COUNT=$(grep -c . "$FILELIST" || true)
if [ "${TARGET_COUNT:-0}" -eq 0 ]; then
  echo "secret-scan: no files to scan. Refusing to report success on an empty" >&2
  echo "  target list: a scanner that checks nothing must not report ok." >&2
  exit 2
fi

FOUND=0
SCANNED=0
PATTERNS=0

while IFS= read -r secret; do
  # Skip empties and anything too short to be meaningful, which would produce
  # false positives against ordinary prose.
  [ -n "$secret" ] || continue
  [ "${#secret}" -ge 4 ] || continue
  PATTERNS=$((PATTERNS + 1))

  # -F fixed string, -I skip binary, -n line numbers. The value itself is never
  # printed: only the file and line are reported.
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    file="${hit%%:*}"
    rest="${hit#*:}"
    lineno="${rest%%:*}"
    echo "secret-scan: FAIL  $file:$lineno contains a value from ARCEN_SECRET_VALUES"
    FOUND=1
  done < <(grep -F -I -n -f /dev/stdin -- $(tr '\n' ' ' < "$FILELIST") 2>/dev/null <<<"$secret" \
           | grep -v '^scripts/review/secret-scan.sh:')
done <<< "$ARCEN_SECRET_VALUES"

SCANNED="$TARGET_COUNT"

if [ "$FOUND" -eq 0 ]; then
  echo "secret-scan: ok  $SCANNED paths scanned against $PATTERNS secret patterns, none found"
else
  echo "secret-scan: a credential is present in the tree. Remove it before committing." >&2
  echo "  Values are deliberately not printed. Use the file and line above." >&2
fi

exit "$FOUND"
