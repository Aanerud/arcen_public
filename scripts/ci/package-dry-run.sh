#!/usr/bin/env bash
set -euo pipefail

if (( $# == 0 )); then
    echo "usage: $0 <arcen-package>..." >&2
    exit 2
fi

output_dir=${ARCEN_PACKAGE_DRY_RUN_DIR:-target/package-dry-run}
mkdir -p "$output_dir"

for package in "$@"; do
    if [[ ! "$package" =~ ^arcen-[a-z0-9-]+$ ]]; then
        echo "invalid Arcen package name: $package" >&2
        exit 2
    fi

    manifest="$output_dir/$package.files"
    cargo package --locked --list -p "$package" > "$manifest"

    grep -Fxq 'Cargo.toml' "$manifest"
    grep -Fxq 'LICENSE' "$manifest"
    if grep -Eq '(^/|(^|/)\.\.(/|$))' "$manifest"; then
        echo "$package includes a path outside its package root" >&2
        exit 1
    fi
done
