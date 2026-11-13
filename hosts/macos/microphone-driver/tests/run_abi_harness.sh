#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
DRIVER="$TARGET_DIR/debug/deps/libarcen_microphone_driver.dylib"
HARNESS="$TARGET_DIR/debug/arcen-microphone-abi-harness"
cc -Wall -Wextra -Werror -framework CoreAudio -framework CoreFoundation "$HERE/abi_harness.c" -o "$HARNESS"
"$HARNESS" "$DRIVER"
