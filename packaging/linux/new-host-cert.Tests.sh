#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
# The helper is a front end for `arcen-pier new-host-cert`; these check that it
# passes everything through unchanged and fails plainly. The transaction itself
# is tested in hosts/linux/src/host_cert.rs.
set -euo pipefail
SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$SCRIPT_DIRECTORY/new-host-cert.sh"
WORK="$SCRIPT_DIRECTORY/.new-host-cert-test-$$"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
mkdir -p "$WORK"

FAKE="$WORK/arcen-pier"
cat >"$FAKE" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$FAKE_RECORD"
exit "${FAKE_EXIT:-0}"
EOF
chmod +x "$FAKE"
export FAKE_RECORD="$WORK/arguments"

expect_arguments() {
  local expected="$1"
  local actual
  actual="$(paste -sd' ' "$FAKE_RECORD")"
  [[ "$actual" == "$expected" ]] || {
    echo "expected arguments: $expected" >&2
    echo "actual arguments:   $actual" >&2
    exit 1
  }
}

# Every argument reaches `arcen-pier new-host-cert` unchanged.
ARCEN_PIER_BIN="$FAKE" "$HELPER" --renew --adopt-legacy --directory "$WORK/tls" \
  --dns pier.example.internal --ip 203.0.113.7
expect_arguments "new-host-cert --renew --adopt-legacy --directory $WORK/tls --dns pier.example.internal --ip 203.0.113.7"

# ARCEN_TLS_DIRECTORY supplies the directory only when none is given.
ARCEN_PIER_BIN="$FAKE" ARCEN_TLS_DIRECTORY="$WORK/env-tls" "$HELPER" --new-key
expect_arguments "new-host-cert --directory $WORK/env-tls --new-key"
ARCEN_PIER_BIN="$FAKE" ARCEN_TLS_DIRECTORY="$WORK/env-tls" "$HELPER" --directory "$WORK/given"
expect_arguments "new-host-cert --directory $WORK/given"

# The Pier beside the helper is found without configuration.
cp "$HELPER" "$WORK/new-host-cert.sh"
env -u ARCEN_PIER_BIN "$WORK/new-host-cert.sh"
expect_arguments "new-host-cert"

# The Pier's exit status is the helper's.
if ARCEN_PIER_BIN="$FAKE" FAKE_EXIT=3 "$HELPER" --renew; then
  echo "a failing Pier must fail the helper" >&2
  exit 1
else
  status=$?
  [[ $status -eq 3 ]] || { echo "exit status $status, expected 3" >&2; exit 1; }
fi

# Help is the helper's own and runs nothing.
rm -f "$FAKE_RECORD"
ARCEN_PIER_BIN="$FAKE" "$HELPER" --help | grep -q 'arcen-pier new-host-cert'
[[ ! -e "$FAKE_RECORD" ]] || { echo "--help must not run the Pier" >&2; exit 1; }

# No Pier: a plain refusal.
if ARCEN_PIER_BIN="$WORK/missing" "$HELPER" 2>"$WORK/error"; then
  echo "a missing Pier must fail" >&2
  exit 1
fi
grep -q 'arcen-pier not found' "$WORK/error"

echo "new-host-cert fixtures passed"
