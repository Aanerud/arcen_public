#!/usr/bin/env bash
set -euo pipefail

failed=false
search_roots=(.github/workflows)
if [[ -d .github/actions ]]; then
    search_roots+=(.github/actions)
fi

while IFS= read -r -d '' file; do
    while IFS= read -r reference; do
        reference=${reference%%#*}
        reference=${reference%"${reference##*[![:space:]]}"}
        if [[ "$reference" == ./* ]]; then
            continue
        fi
        if [[ ! "$reference" =~ @[0-9a-f]{40}$ ]]; then
            echo "$file: action is not pinned to a full commit SHA: $reference" >&2
            failed=true
        fi
    done < <(
        sed -n \
            -e 's/^[[:space:]]*uses:[[:space:]]*//p' \
            -e 's/^[[:space:]]*-[[:space:]]*uses:[[:space:]]*//p' \
            "$file"
    )
done < <(
    find "${search_roots[@]}" -type f \
        \( -name '*.yml' -o -name '*.yaml' \) -print0
)

if [[ "$failed" == "true" ]]; then
    exit 1
fi
