#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

set -euo pipefail

for value in GLOBAL UNKNOWN LINUX_HOST WINDOWS_HOST MACOS_CLIENT WINDOWS_CLIENT; do
    case "${!value:-false}" in
        true | false) ;;
        *)
            echo "$value must be true or false" >&2
            exit 2
            ;;
    esac
done

targets=()

add_target() {
    targets+=("$1")
}

if [[ "${UNKNOWN:-false}" == "true" ]]; then
    GLOBAL=true
fi

if [[ "${GLOBAL:-false}" == "true" || "${LINUX_HOST:-false}" == "true" ]]; then
    add_target '{"name":"Linux host","runner":"ubuntu-latest","packages":"arcen-host-linux","artifact":"linux-host"}'
fi
if [[ "${GLOBAL:-false}" == "true" || "${WINDOWS_HOST:-false}" == "true" ]]; then
    add_target '{"name":"Windows host","runner":"windows-latest","packages":"arcen-host-windows","artifact":"windows-host"}'
fi
if [[ "${GLOBAL:-false}" == "true" || "${MACOS_CLIENT:-false}" == "true" ]]; then
    add_target '{"name":"macOS client","runner":"macos-latest","packages":"arcen-client-macos","artifact":"macos-client"}'
fi
if [[ "${GLOBAL:-false}" == "true" || "${WINDOWS_CLIENT:-false}" == "true" ]]; then
    add_target '{"name":"Windows client","runner":"windows-latest","packages":"arcen-client-windows","artifact":"windows-client"}'
fi

# Unknown paths are integration changes until they are classified explicitly.
if (( ${#targets[@]} == 0 )); then
    add_target '{"name":"Linux host","runner":"ubuntu-latest","packages":"arcen-host-linux","artifact":"linux-host"}'
    add_target '{"name":"Windows host","runner":"windows-latest","packages":"arcen-host-windows","artifact":"windows-host"}'
    add_target '{"name":"macOS client","runner":"macos-latest","packages":"arcen-client-macos","artifact":"macos-client"}'
    add_target '{"name":"Windows client","runner":"windows-latest","packages":"arcen-client-windows","artifact":"windows-client"}'
fi

matrix='{"include":['
separator=''
for target in "${targets[@]}"; do
    matrix+="${separator}${target}"
    separator=','
done
matrix+=']}'

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    printf 'targets=%s\n' "$matrix" >> "$GITHUB_OUTPUT"
else
    printf '%s\n' "$matrix"
fi
