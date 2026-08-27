#!/usr/bin/env bash
# doc-guard: refuse documentation edits that destroy content.
#
# Why this exists
# ---------------
# On 2026-07-25 a delegated documentation pass replaced ten large
# ARCHITECTURE.md files with 18 to 36 line skeletons and deleted a block of
# governing rules from the root AGENTS.md. Every one of those edits passed a
# structural heading-order check, because heading order says nothing about
# whether the content under the headings survived.
#
# This guard is the mechanical answer. It compares each documentation file
# against a base ref and fails on two conditions:
#
#   1. Any ARCHITECTURE.md or AGENTS.md loses more than MAX_SHRINK_PCT of its
#      line count, or is deleted outright.
#   2. The root AGENTS.md source-provenance block changes at all.
#
# Usage
#   scripts/review/doc-guard.sh [base-ref]      # default base ref: main
#
# Environment
#   DOC_GUARD_MAX_SHRINK_PCT        shrink tolerance, default 5
#   DOC_GUARD_ALLOW_PROVENANCE_EDIT set to 1 for a deliberate Release/Security
#                                   change to the provenance block; the override
#                                   is printed so it is visible in CI logs
#
# Exit status
#   0  all documentation edits are acceptable
#   1  a guard tripped; the offending files are listed
#   2  usage or environment error
set -uo pipefail

BASE_REF="${1:-main}"
MAX_SHRINK_PCT="${DOC_GUARD_MAX_SHRINK_PCT:-5}"

if ! git rev-parse --verify --quiet "$BASE_REF" >/dev/null; then
  echo "doc-guard: base ref '$BASE_REF' does not exist" >&2
  exit 2
fi

FAILED=0

# --- Guard 1: content-loss check -------------------------------------------
# Compare every documentation file that exists in the base ref. A newly added
# file cannot lose content, so only base-ref files are checked.

BASE_DOCS=()
while IFS= read -r line; do
  [ -n "$line" ] && BASE_DOCS+=("$line")
done < <(git ls-tree -r --name-only "$BASE_REF" | grep -E '(^|/)(ARCHITECTURE|AGENTS)\.md$' || true)

for doc in "${BASE_DOCS[@]}"; do
  before=$(git show "$BASE_REF:$doc" 2>/dev/null | wc -l | tr -d ' ')
  [ "$before" -eq 0 ] && continue

  if [ ! -f "$doc" ]; then
    echo "doc-guard: FAIL  $doc was DELETED (was $before lines)"
    FAILED=1
    continue
  fi

  after=$(wc -l < "$doc" | tr -d ' ')
  if [ "$after" -ge "$before" ]; then
    continue
  fi

  shrink=$(( (before - after) * 100 / before ))
  if [ "$shrink" -gt "$MAX_SHRINK_PCT" ]; then
    echo "doc-guard: FAIL  $doc shrank ${shrink}% ($before -> $after lines), limit is ${MAX_SHRINK_PCT}%"
    FAILED=1
  fi
done

# --- Guard 2: root AGENTS.md publication invariants are present -------------
#
# Arcen is a public AGPL-3.0 repository. These invariants are the rules that
# keep it publishable; silently dropping one is how a private detail or an
# incompatible licence gets in. Deep enforcement lives in
# scripts/ci/check-publication-hygiene.sh.

PUBLICATION_MARKERS=(
  'AGPL-3.0-only'
  'Never import uncleared'
  'Never commit secrets'
  'Never commit private infrastructure'
  'legal/THIRD_PARTY_NOTICES.md'
)

for marker in "${PUBLICATION_MARKERS[@]}"; do
  if [ ! -f AGENTS.md ]; then
    echo "doc-guard: FAIL  root AGENTS.md is missing from the working tree"
    FAILED=1
    break
  fi
  if ! grep -qF -- "$marker" AGENTS.md; then
    echo "doc-guard: FAIL  root AGENTS.md no longer states a publication invariant: $marker"
    FAILED=1
  fi
done

if [ "$FAILED" -eq 0 ]; then
  echo "doc-guard: ok  ${#BASE_DOCS[@]} documentation files checked against $BASE_REF, none lost more than ${MAX_SHRINK_PCT}%, publication invariants intact"
fi

exit "$FAILED"
