#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

linux=$(GITHUB_OUTPUT= GLOBAL=false LINUX_HOST=true "$script_dir/component-matrix.sh")
jq -e '.include | length == 1 and .[0].artifact == "linux-host"' <<< "$linux" >/dev/null

global=$(GITHUB_OUTPUT= GLOBAL=true "$script_dir/component-matrix.sh")
jq -e '.include | length == 5' <<< "$global" >/dev/null

mixed=$(GITHUB_OUTPUT= UNKNOWN=true LINUX_HOST=true "$script_dir/component-matrix.sh")
jq -e '.include | length == 5' <<< "$mixed" >/dev/null

fallback=$(GITHUB_OUTPUT= "$script_dir/component-matrix.sh")
jq -e '.include | length == 5' <<< "$fallback" >/dev/null
