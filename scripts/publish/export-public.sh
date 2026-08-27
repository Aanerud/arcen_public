#!/usr/bin/env bash
# Export the publishable tree to the public repository.
#
# Development happens in the private repository, which keeps its history. That
# history cannot be published: it holds lab addresses, the prior project's name,
# and the removed licensing stack, and GitHub keeps 124 pull-request refs that
# the owner cannot delete. So publication is a one-way export rather than a
# migration, and this script is the only sanctioned way to perform it.
#
# The first export creates a root commit. Later releases add ordinary commits on
# top of it, so the public repository is never force-pushed: contributors' forks,
# issues and commit links stay valid, and the public history is a series of
# release snapshots starting at the first published version.
#
# Usage:
#   scripts/publish/export-public.sh --tag v0.9.8 [--into <clone>] [--remote <url>] [--dry-run]
#
# The export refuses to run unless the working tree is clean, the hygiene gate
# passes with the private denylist present, and the third-party inventory
# matches the lockfile. It never pushes; it prints the commands to review first.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$repo"

tag=""
remote="https://github.com/Aanerud/arcen_public.git"
into=""
dry_run=0
while (($#)); do
    case "$1" in
        --tag)
            tag=${2:-}
            shift 2
            ;;
        --remote)
            remote=${2:-}
            shift 2
            ;;
        --into)
            into=${2:-}
            shift 2
            ;;
        --dry-run)
            dry_run=1
            shift
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

[[ -n "$tag" ]] || {
    echo "error: --tag is required, e.g. --tag v0.9.8" >&2
    exit 2
}

fail() {
    echo "REFUSING TO EXPORT: $*" >&2
    exit 1
}

# 1. The tree must be exactly what was reviewed and tested.
[[ -z "$(git status --porcelain)" ]] ||
    fail "the working tree has uncommitted changes; commit or stash them first"

# 2. The hygiene gate must pass WITH the private denylist. Publishing without
#    the private checks having run is the failure this whole exercise exists to
#    prevent, so a missing denylist is fatal here rather than a notice.
#
#    The denylist path is pinned rather than inherited. `check-publication-hygiene.sh`
#    honours `ARCEN_PUBLICATION_DENYLIST` so a contributor can point it at their
#    own file, but that override also let the export be defeated: setting it to
#    any existing file — `ARCEN_PUBLICATION_DENYLIST=LICENSE` — satisfied
#    "denylist present" while running zero private checks, and the gate reported
#    passed. An export must not be able to opt out of the checks that make it
#    safe.
readonly canonical_denylist=.claude/publication-denylist.txt
[[ -f "$canonical_denylist" ]] ||
    fail "the private denylist $canonical_denylist is missing"

# It must also be the real thing, not an empty or truncated file that would
# satisfy the existence check while forbidding nothing.
denylist_rules=$(grep -cE '^[^#[:space:]].*\|' "$canonical_denylist" || true)
((denylist_rules >= 5)) ||
    fail "$canonical_denylist has only $denylist_rules rules; expected the full private set"

ARCEN_PUBLICATION_DENYLIST="$canonical_denylist" \
    ARCEN_REQUIRE_PRIVATE_DENYLIST=1 \
    bash scripts/ci/check-publication-hygiene.sh ||
    fail "publication hygiene gate failed"

# 3. The third-party inventory must match the locked dependency graph.
python3 scripts/generate-third-party-notices.py --check ||
    fail "third-party notices do not match Cargo.lock"

# 4. The tag must match the workspace version.
workspace_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[[ "$tag" == "v$workspace_version" ]] ||
    fail "tag $tag does not match workspace version $workspace_version"

echo "==> exporting $workspace_version as $tag to $remote"

# The export is built from the TRACKED file set. Never `git add -A` over this
# directory: it holds refference/, keys/, dist/, target/ and .claude/, and an
# ignore rule is a weaker guarantee than never offering the file in the first
# place.
staging=$(mktemp -d "${TMPDIR:-/tmp}/arcen-export-XXXXXX")
cleanup() { rm -rf "$staging"; }
trap cleanup EXIT

git archive --format=tar HEAD | tar -x -C "$staging"

# Prove the obvious rather than trusting it. `git archive` emits only tracked
# files, so any of these appearing means something is tracked that must not be.
for forbidden in refference keys dist-installers .claude .local "Arcen Deck.app"; do
    [[ ! -e "$staging/$forbidden" ]] ||
        fail "$forbidden reached the export staging directory"
done

# dist/ is a special case: README.md is tracked and documents how release
# artefacts are built, but no binary in that directory ever is. Allow the file
# and nothing else, rather than allowing the directory.
if [[ -e "$staging/dist" ]]; then
    unexpected=$(find "$staging/dist" -mindepth 1 ! -name README.md)
    [[ -z "$unexpected" ]] || {
        printf '%s\n' "$unexpected" >&2
        fail "dist/ contains something other than README.md"
    }
fi

if find "$staging" -name '*.private.md' -print -quit | grep -q .; then
    fail "a *.private.md file reached the export staging directory"
fi

echo "==> staged $(find "$staging" -type f | wc -l | tr -d ' ') tracked files"

if ((dry_run)); then
    echo "==> dry run: nothing was built; staging tree discarded"
    exit 0
fi

# Build the public commit somewhere that is not this repository, so the private
# index and HEAD are never touched. `--into` targets an existing clone, which is
# what you want when iterating; without it a throwaway clone is used.
if [[ -n "$into" ]]; then
    public=$(cd "$into" && pwd) ||
        fail "--into path does not exist: $into"
    [[ -d "$public/.git" ]] ||
        fail "--into path is not a git repository: $public"
    [[ "$public" != "$repo" ]] ||
        fail "--into must not be this repository"
    [[ -z "$(git -C "$public" status --porcelain)" ]] ||
        fail "the target repository has uncommitted changes: $public"
    echo "==> exporting into existing clone $public"
else
    public=$(mktemp -d "${TMPDIR:-/tmp}/arcen-public-XXXXXX")
    git init -q "$public"
    git -C "$public" remote add origin "$remote"
fi

# If the public repository already has history, continue it. Only the very first
# export is a root commit; force-pushing a public repository breaks every fork
# and every link into it.
if git -C "$public" rev-parse --verify -q HEAD >/dev/null 2>&1 ||
    git -C "$public" fetch -q --depth 1 origin main 2>/dev/null; then
    git -C "$public" rev-parse --verify -q main >/dev/null 2>&1 ||
        git -C "$public" checkout -q -b main FETCH_HEAD
    git -C "$public" checkout -q main 2>/dev/null || true
    # Replace the tree wholesale: a file deleted here must disappear there.
    find "$public" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
    echo "==> continuing existing public history"
else
    git -C "$public" checkout -q -b main
    echo "==> first export: creating a root commit"
fi

cp -R "$staging"/. "$public"/
git -C "$public" add -A
git -C "$public" commit -q -m "Arcen $tag

Arcen is a remote-desktop system: a Pier streams a real desktop to a Deck over a
direct QUIC/TLS 1.3 connection. Free software under the GNU AGPL-3.0.

This repository publishes release snapshots. Development history lives
elsewhere and is not part of this tree."
git -C "$public" tag -a "$tag" -m "Arcen $tag"

echo "==> built $(git -C "$public" rev-parse --short HEAD) in $public"
echo
echo "Review it, then publish with:"
echo "    git -C $public push -u origin main"
echo "    git -C $public push origin $tag"
echo
echo "Nothing has been pushed."
