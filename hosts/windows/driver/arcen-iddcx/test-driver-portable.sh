#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
SOURCE="$ROOT/hosts/windows/driver/arcen-iddcx"
OUT="$ROOT/target/arcen-iddcx-portable-test"
rm -rf "$OUT"
mkdir -p "$OUT"
trap 'rm -rf "$OUT"' EXIT

"${CXX:-c++}" -std=c++17 -Wall -Wextra -Werror -pedantic \
  "$SOURCE/arcen_iddcx_model.cpp" \
  "$SOURCE/arcen_iddcx_model_test.cpp" \
  -o "$OUT/arcen-iddcx-model-test"
"$OUT/arcen-iddcx-model-test"
