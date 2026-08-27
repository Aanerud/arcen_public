#!/usr/bin/env bash
set -euo pipefail

output_dir=${1:-target/supply-chain}
repo_root=$(git rev-parse --show-toplevel)
output_dir=$(mkdir -p "$output_dir" && cd "$output_dir" && pwd)
generated_stem=arcen.cdx
generated_name=arcen.cdx.json

if find "$repo_root" -name "$generated_name" -type f -print -quit | grep -q .; then
    echo "$generated_name already exists; refusing to delete a pre-existing file" >&2
    exit 1
fi

cleanup() {
    find "$repo_root" -name "$generated_name" -type f -delete
}
trap cleanup EXIT

cd "$repo_root"
cargo cyclonedx --all --all-features --format json \
    --spec-version 1.5 --override-filename "$generated_stem"

while IFS= read -r -d '' sbom; do
    relative=${sbom#"$repo_root"/}
    directory=$(dirname "$relative")
    if [[ "$directory" == "." ]]; then
        name=workspace
    else
        name=${directory//\//-}
    fi
    cp "$sbom" "$output_dir/$name.cdx.json"
done < <(find "$repo_root" -name "$generated_name" -type f -print0)

commit=${GITHUB_SHA:-$(git rev-parse HEAD)}
repository=${GITHUB_REPOSITORY:-Aanerud/arcen_public}
workflow_ref=${GITHUB_WORKFLOW_REF:-local}
run_id=${GITHUB_RUN_ID:-local}
lock_digest=$(sha256sum Cargo.lock | cut -d ' ' -f 1)

jq -cn \
    --arg repository "$repository" \
    --arg commit "$commit" \
    --arg workflow_ref "$workflow_ref" \
    --arg run_id "$run_id" \
    --arg lock_digest "$lock_digest" \
    '{
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{
            "name": $repository,
            "digest": {"gitCommit": $commit}
        }],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {
            "buildDefinition": {
                "buildType": "https://github.com/Attestations/GitHubActionsWorkflow@v1",
                "externalParameters": {
                    "workflow": $workflow_ref,
                    "cargoLockSha256": $lock_digest
                },
                "internalParameters": {}
            },
            "runDetails": {
                "builder": {"id": $workflow_ref},
                "metadata": {"invocationId": $run_id}
            }
        }
    }' > "$output_dir/source-provenance.intoto.jsonl"

(
    cd "$output_dir"
    find . -type f ! -name SHA256SUMS -print0 |
        sort -z |
        xargs -0 sha256sum > SHA256SUMS
)
