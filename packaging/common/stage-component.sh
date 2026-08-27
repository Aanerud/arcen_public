#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=checksums.sh
source "$script_dir/checksums.sh"

if (( $# != 3 )); then
    echo "usage: $0 <target> <package> <output-directory>" >&2
    exit 2
fi

target=$1
package=$2
output_dir=$3

case "$target" in
    linux-host) hook=packaging/linux/build-release.sh ;;
    windows-host | windows-client) hook=packaging/windows/build-release.sh ;;
    macos-client) hook=packaging/macos/build-release.sh ;;
    gateway-service | gateway-relay) hook=packaging/gateway/build-release.sh ;;
    *)
        echo "unsupported release target: $target" >&2
        exit 2
        ;;
esac

if [[ ! -x "$hook" ]]; then
    echo "$hook is not implemented; refusing to stage a placeholder artifact" >&2
    exit 1
fi
if [[ -z "${ARCEN_RELEASE_VERSION:-}" || -z "${ARCEN_RELEASE_COMMIT:-}" ]]; then
    echo "ARCEN_RELEASE_VERSION and ARCEN_RELEASE_COMMIT are required" >&2
    exit 1
fi

rm -rf "$output_dir"
mkdir -p "$output_dir"
"$hook" \
    --package "$package" \
    --version "$ARCEN_RELEASE_VERSION" \
    --commit "$ARCEN_RELEASE_COMMIT" \
    --output "$output_dir"

if ! find "$output_dir" -type f -print -quit | grep -q .; then
    echo "$hook produced no release files" >&2
    exit 1
fi
if find "$output_dir" -type l -print -quit | grep -q .; then
    echo "$hook produced a symbolic link; release artifacts must be self-contained" >&2
    exit 1
fi

arcen_write_checksums "$output_dir"
