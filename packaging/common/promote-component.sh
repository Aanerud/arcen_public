#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=checksums.sh
source "$script_dir/checksums.sh"

if (( $# != 3 )); then
    echo "usage: $0 <target> <staged-directory> <signed-directory>" >&2
    exit 2
fi

target=$1
staged_dir=$2
signed_dir=$3

case "$target" in
    linux-host) hook=packaging/linux/sign-release.sh ;;
    windows-host | windows-client) hook=packaging/windows/sign-release.sh ;;
    macos-client) hook=packaging/macos/sign-and-notarize-release.sh ;;
    gateway-service | gateway-relay) hook=packaging/gateway/sign-release.sh ;;
    *)
        echo "unsupported release target: $target" >&2
        exit 2
        ;;
esac

if [[ ! -f "$staged_dir/SHA256SUMS" ]]; then
    echo "staged artifact has no SHA256SUMS" >&2
    exit 1
fi
arcen_check_checksums "$staged_dir"
staged_manifest_digest=$(arcen_checksum_digest "$staged_dir/SHA256SUMS")
if [[ ! -x "$hook" ]]; then
    echo "$hook is not implemented; refusing to promote an unsigned artifact" >&2
    exit 1
fi

rm -rf "$signed_dir"
mkdir -p "$signed_dir"
"$hook" --input "$staged_dir" --output "$signed_dir"

if ! find "$signed_dir" -type f -print -quit | grep -q .; then
    echo "$hook produced no signed release files" >&2
    exit 1
fi
if find "$signed_dir" -type l -print -quit | grep -q .; then
    echo "$hook produced a symbolic link; signed artifacts must be self-contained" >&2
    exit 1
fi

arcen_write_checksums "$signed_dir"
mv "$signed_dir/SHA256SUMS" "$signed_dir/SIGNED-SHA256SUMS"
signed_manifest_digest=$(arcen_checksum_digest "$signed_dir/SIGNED-SHA256SUMS")
jq -n \
    --arg target "$target" \
    --arg staged "$staged_manifest_digest" \
    --arg signed "$signed_manifest_digest" \
    '{
        schemaVersion: 1,
        target: $target,
        operation: "sign-or-notarize-existing-staged-artifact",
        stagedChecksumsSha256: $staged,
        signedChecksumsSha256: $signed
    }' > "$signed_dir/PROMOTION.json"
arcen_write_checksums "$signed_dir"
