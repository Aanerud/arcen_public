#!/usr/bin/env bash
# redact.sh: stdin to stdout filter that removes operational secrets.
#
# Every transcript captured from a remote machine passes through this before it
# is written under the session state directory. The rule from the operator
# reply is "scrub before you persist", so redaction happens in the pipeline, not
# as a later cleanup pass that someone can forget.
#
# Usage
#   export ARCEN_SECRET_VALUES=...        # private, per-machine list
#   ssh host 'some command' 2>&1 | scripts/review/redact.sh > evidence.txt
#
# Fails closed: if the secret list is empty the filter refuses to run rather
# than passing text through unredacted.
set -uo pipefail

if [ -z "${ARCEN_SECRET_VALUES:-}" ]; then
  echo "redact: ARCEN_SECRET_VALUES is empty; refusing to pass text through unredacted" >&2
  exit 2
fi

# Build a sed script of fixed-string replacements. Longest first, so that a
# secret which contains another secret as a substring is replaced whole.
SEDS=$(mktemp)
trap 'rm -f "$SEDS"' EXIT

while IFS= read -r secret; do
  [ -n "$secret" ] || continue
  [ "${#secret}" -ge 4 ] || continue
  printf '%s\n' "$secret"
done <<< "$ARCEN_SECRET_VALUES" \
  | awk '{ print length($0)"\t"$0 }' | sort -rn | cut -f2- \
  | while IFS= read -r secret; do
      # Escape every character that is special to sed, so the match is literal.
      esc=$(printf '%s' "$secret" | sed -e 's/[]\/$*.^[]/\\&/g')
      printf 's/%s/[REDACTED]/g\n' "$esc" >> "$SEDS"
    done

if [ ! -s "$SEDS" ]; then
  echo "redact: no usable secret patterns; refusing to pass text through" >&2
  exit 2
fi

sed -f "$SEDS"
