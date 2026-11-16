#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
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
#
# The two Decks share code only through shared/deck-core (ADR 0024):
# clients/windows has one helper edge (the unprivileged USB component
# client), and neither Deck may depend on the other. The Windows USB
# component's service and prepare tool (clients/windows/usb/{service,prepare})
# run privileged: their resolved graph (every target, normal and build edges,
# transitively) must contain no Deck, transport, media, UI, GPU or async
# runtime package (checked below).
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
clients/macos/usb-helper clients/macos/usb-iousbhost
clients/macos/usb-helper clients/macos/secure-dir
clients/macos/usb-iousbhost clients/macos/secure-dir
clients/windows clients/windows/usb/client
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
        hosts/* | clients/* | tests/*)
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

privileged_forbidden='^(arcen-deck-core|arcen-deck-windows|arcen-deck-macos|arcen-transport|arcen-media|egui|egui-wgpu|egui-winit|eframe|epaint|wgpu|wgpu-core|wgpu-hal|winit|tokio|quinn|quinn-proto) v'
while IFS=$'\t' read -r package manifest; do
    from=${manifest#"$root"/}
    from=${from%/Cargo.toml}
    # The Deck-side client (clients/windows/usb/client) runs inside the
    # unprivileged Deck and is not privileged.
    [[ "$from" == clients/windows/usb/service || "$from" == clients/windows/usb/prepare ]] || continue
    if found=$(cargo tree --locked -p "$package" -e normal,build --target all --prefix none |
        grep -E "$privileged_forbidden"); then
        echo "privileged USB component $package ($from) links $(sort -u <<< "$found" | paste -sd, -)" >&2
        status=1
    fi
done < <(jq -r '.packages[] | [.name, .manifest_path] | @tsv' <<< "$metadata")

if ((status == 0)); then
    echo "workspace boundaries: ok"
fi
exit "$status"
