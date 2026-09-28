#!/usr/bin/env bash
# Prove the dependency direction the architecture relies on.
#
# - Every package is named arcen-*.
# - A shared crate depends on shared crates only (and third-party crates).
# - A product crate depends on shared crates, or on one of the product's own
#   helper crates listed below, and on nothing else in the tree: no host on a
#   client, no client on a host, no host on another host's internals.
#
# The allowlist is the reviewed set of product-internal edges. Adding an edge
# is a deliberate change to this file, not something a manifest can do alone.
set -euo pipefail

metadata=$(cargo metadata --locked --no-deps --format-version 1)
root=$(jq -r '.workspace_root' <<< "$metadata")

# "<dependent manifest dir> <dependency manifest dir>", relative to the root.
allowed_edges='
hosts/linux hosts/capenc
hosts/linux hosts/audiocap
hosts/linux hosts/input-helper
hosts/windows hosts/capenc
hosts/windows hosts/windows/cp-ipc
hosts/windows hosts/windows/iddcx-provider
hosts/windows/credential-provider hosts/windows/cp-ipc
clients/macos clients/macos/usb-helper
'

jq -e 'all(.packages[]; .name | startswith("arcen-"))' <<< "$metadata" >/dev/null ||
    { echo "a package is not named arcen-*" >&2; exit 1; }

status=0
while IFS=$'\t' read -r package manifest dependency path; do
    from=${manifest#"$root"/}
    from=${from%/Cargo.toml}
    to=${path#"$root"/}
    case "$from" in
        shared/*)
            [[ "$to" == shared/* ]] && continue
            echo "shared crate $package ($from) depends on $dependency ($to)" >&2
            status=1
            ;;
        hosts/* | clients/* | gateway/* | tests/*)
            [[ "$to" == shared/* ]] && continue
            if grep -qxF "$from $to" <<< "$allowed_edges"; then
                continue
            fi
            echo "product crate $package ($from) depends on $dependency ($to), which is neither shared nor an allowed helper" >&2
            status=1
            ;;
    esac
done < <(jq -r '.packages[] | .name as $n | .manifest_path as $m
    | .dependencies[] | select(.path != null) | [$n, $m, .name, .path] | @tsv' <<< "$metadata")

if ((status == 0)); then
    echo "workspace boundaries: ok"
fi
exit "$status"
