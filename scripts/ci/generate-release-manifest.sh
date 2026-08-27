#!/usr/bin/env bash
set -euo pipefail

if (( $# != 4 )); then
    echo "usage: $0 <version> <commit> <artifact-directory> <manifest>" >&2
    exit 2
fi

version=$1
commit=$2
artifact_dir=$3
manifest=$4
entries=$(mktemp)
trap 'rm -f "$entries"' EXIT

while IFS= read -r -d '' file; do
    relative=${file#"$artifact_dir"/}
    digest=$(sha256sum "$file" | cut -d ' ' -f 1)
    size=$(wc -c < "$file" | tr -d ' ')
    jq -cn \
        --arg name "$relative" \
        --arg sha256 "$digest" \
        --argjson size "$size" \
        '{name: $name, size: $size, sha256: $sha256}' >> "$entries"
done < <(
    find "$artifact_dir" -type f \
        ! -name release-manifest.json \
        ! -name release-manifest.json.sig \
        ! -name SHA256SUMS \
        -print0 |
        sort -z
)

jq -s \
    --arg version "$version" \
    --arg commit "$commit" \
    --arg repository "${GITHUB_REPOSITORY:-Aanerud/arcen_public}" \
    --arg workflow_ref "${GITHUB_WORKFLOW_REF:-local}" \
    '{
        schemaVersion: 1,
        product: "arcen",
        version: $version,
        repository: $repository,
        commit: $commit,
        promotionWorkflow: $workflow_ref,
        artifacts: .
    }' "$entries" > "$manifest"
