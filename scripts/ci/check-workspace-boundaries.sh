#!/usr/bin/env bash
set -euo pipefail

metadata=$(cargo metadata --locked --no-deps --format-version 1)

jq -e '
    all(.packages[];
        (.name | startswith("arcen-"))
        and .edition == "2024"
        and .rust_version == "1.85"
    )
' <<< "$metadata" >/dev/null

jq -e '
    all(.packages[];
        if (.manifest_path | contains("/shared/"))
        then all(.dependencies[]; .path == null or (.path | contains("/shared/")))
        else true
        end
    )
' <<< "$metadata" >/dev/null

jq -e '
    all(.packages[];
        if (.manifest_path | endswith("/clients/windows/Cargo.toml"))
        then all(.dependencies[];
            .path == null
            or (.path | contains("/shared/"))
            or (.path | endswith("/clients/windows/native"))
        )
        elif (.manifest_path | test("/(hosts|clients|gateway|tests)/"))
        then all(.dependencies[]; .path == null or (.path | contains("/shared/")))
        else true
        end
    )
' <<< "$metadata" >/dev/null
