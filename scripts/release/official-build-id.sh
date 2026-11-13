#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
#
# Print the build stamp of an official Arcen build.
#
# Official builds are made from a clean checkout of a published release tag,
# so the source revision they carry is a commit anyone can fetch from the
# public repository. This prints the environment the build scripts read
# (shared/protocol/src/build_identity.rs):
#
#   ARCEN_BUILD_ID=official-<YYYYMMDDTHHMMSSZ>-<tag>-<12 hex of the commit>
#   ARCEN_SOURCE_REVISION=<full commit>
#
# Usage:  eval "$(scripts/release/official-build-id.sh)"
#
# It refuses to run on a dirty tree, on a commit without an exact `v*` tag, or
# when the tag does not match the workspace version.
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
cd "$repo"

fail() {
    echo "official-build-id: $*" >&2
    exit 1
}

[[ -z "$(git status --porcelain --untracked-files=no)" ]] ||
    fail "the checkout has uncommitted changes; official builds come from a clean tag"

tag=$(git describe --exact-match --tags --match 'v*' HEAD 2>/dev/null) ||
    fail "HEAD is not an exact release tag (v*); check out the published tag first"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[[ "$tag" == "v$version" || "$tag" == "v$version"-* ]] ||
    fail "tag $tag does not match workspace version $version"

revision=$(git rev-parse HEAD)
stamp=$(date -u +%Y%m%dT%H%M%SZ)

printf 'export ARCEN_BUILD_ID=%s\n' "official-$stamp-$tag-${revision:0:12}"
printf 'export ARCEN_SOURCE_REVISION=%s\n' "$revision"
