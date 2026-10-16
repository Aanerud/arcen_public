#!/usr/bin/env bash
set -euo pipefail
exec bash "$(dirname "${BASH_SOURCE[0]}")/verify-vendored-source.sh" opusic-sys-0.7.3-arcen1
