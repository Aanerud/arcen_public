#!/usr/bin/env bash
# Install the nightly-run git hooks into this clone.
# Hooks are not versioned by git itself, so this makes the gate reproducible.
set -euo pipefail
REPO="$(git rev-parse --show-toplevel)"
install -m 0755 "$REPO/scripts/review/pre-commit" "$REPO/.git/hooks/pre-commit"
echo "installed: .git/hooks/pre-commit -> secret-scan + doc-guard"
