#!/usr/bin/env bash
set -euo pipefail

ref=${1:-}
if [[ -z "$ref" || -z "${GITHUB_REPOSITORY:-}" || -z "${GH_TOKEN:-}" ]]; then
    echo "usage: GH_TOKEN=... GITHUB_REPOSITORY=owner/repo $0 <ref>" >&2
    exit 2
fi

default_branch=$(gh api "repos/$GITHUB_REPOSITORY" --jq .default_branch)
sha=$(gh api "repos/$GITHUB_REPOSITORY/commits/$ref" --jq .sha)
status=$(gh api "repos/$GITHUB_REPOSITORY/compare/$sha...$default_branch" --jq .status)

case "$status" in
    ahead | identical) ;;
    *)
        echo "$ref resolves to $sha, which is not contained in $default_branch" >&2
        exit 1
        ;;
esac

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    printf 'sha=%s\nshort_sha=%s\n' "$sha" "${sha:0:12}" >> "$GITHUB_OUTPUT"
else
    printf '%s\n' "$sha"
fi
