#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$SCRIPT_DIRECTORY/new-host-cert.sh"
WORK="$SCRIPT_DIRECTORY/.new-host-cert-test-$$"
FAKE_BIN="$WORK/bin"
OPENSSL_LOG="$WORK/openssl.log"
OPENSSL_COUNTER="$WORK/openssl.counter"
REAL_MV="$(command -v mv)"
REAL_OPENSSL="$(command -v openssl || true)"

cleanup() {
  rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$FAKE_BIN"
PLATFORM="$(uname -s)"

if [[ "$PLATFORM" == MINGW* || "$PLATFORM" == CYGWIN* ]]; then
  cat >"$FAKE_BIN/install" <<'EOF'
#!/usr/bin/env bash
set -e
directory="${@: -1}"
mkdir -p "$directory"
EOF
  cat >"$FAKE_BIN/chmod" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  cat >"$FAKE_BIN/sync" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  cat >"$FAKE_BIN/flock" <<'EOF'
#!/usr/bin/env bash
[[ "${ARCEN_TEST_FLOCK_FAIL:-0}" != "1" ]]
EOF
  chmod +x "$FAKE_BIN/install" "$FAKE_BIN/chmod" "$FAKE_BIN/sync" "$FAKE_BIN/flock"
fi

cat >"$FAKE_BIN/openssl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%q ' "$@" >>"$OPENSSL_LOG"
printf '\n' >>"$OPENSSL_LOG"
command_name="${1:-}"
shift || true
argument_value() {
  local wanted="$1"
  shift
  while (($#)); do
    if [[ "$1" == "$wanted" ]]; then
      printf '%s' "$2"
      return
    fi
    shift
  done
  return 1
}
case "$command_name" in
  genpkey)
    count=0
    [[ ! -f "$OPENSSL_COUNTER" ]] || count="$(cat "$OPENSSL_COUNTER")"
    count=$((count + 1))
    printf '%s\n' "$count" >"$OPENSSL_COUNTER"
    output="$(argument_value -out "$@")"
    printf 'FAKE-P256-KEY-%s\n' "$count" >"$output"
    ;;
  req)
    key="$(argument_value -key "$@")"
    output="$(argument_value -out "$@")"
    printf 'FAKE-CERT\nKEY=%s\nARGS=' "$(cat "$key")" >"$output"
    printf '%q ' "$@" >>"$output"
    printf '\n' >>"$output"
    ;;
  x509)
    input="$(argument_value -in "$@")"
    if [[ " $* " == *" -pubkey "* ]]; then
      sed -n 's/^KEY=/PUB:/p' "$input"
    elif [[ " $* " == *" -fingerprint "* ]]; then
      printf 'sha256 Fingerprint=AA:BB:CC:DD\n'
    else
      grep -q '^FAKE-CERT$' "$input"
    fi
    ;;
  pkey)
    if [[ " $* " == *" -pubin "* ]]; then
      sed 's/^PUB:/DER:/'
    elif [[ " $* " == *" -pubout "* ]]; then
      input="$(argument_value -in "$@")"
      printf 'DER:%s\n' "$(cat "$input")"
    else
      input="$(argument_value -in "$@")"
      grep -q '^FAKE-P256-KEY-' "$input"
    fi
    ;;
  dgst)
    value="$(cat)"
    if [[ " $* " == *" -binary "* ]]; then
      printf 'binary-%s' "$(printf '%s' "$value" | cksum | awk '{print $1}')"
    else
      printf 'SHA2-256(stdin)= %s\n' "$(printf '%s' "$value" | cksum | awk '{print $1}')"
    fi
    ;;
  base64)
    cat >/dev/null
    printf 'ZmFrZS1zcGtp'
    ;;
  *)
    echo "unexpected fake OpenSSL command: $command_name" >&2
    exit 1
    ;;
esac
EOF
chmod +x "$FAKE_BIN/openssl"
export OPENSSL="$FAKE_BIN/openssl" OPENSSL_LOG OPENSSL_COUNTER
export PATH="$FAKE_BIN:$PATH"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_mode() {
  local expected="$1" path="$2" actual
  actual="$(stat -c '%a' "$path")"
  [[ "$actual" == "$expected" ]] || fail "$path mode $actual, expected $expected"
}

PAIR="$WORK/pair"
"$HELPER" --directory "$PAIR" --dns pier.example --ip 192.0.2.10 --ip 2001:db8::10
[[ -s "$PAIR/host.crt" && -s "$PAIR/host.key" ]] || fail "pair not generated"
[[ -s "$PAIR/host.cert-sha256" && -s "$PAIR/host.spki-sha256" ]] || fail "pins not generated"
[[ -s "$PAIR/host.generated-by-arcen" ]] || fail "ownership marker not generated"
if [[ "$PLATFORM" != MINGW* && "$PLATFORM" != CYGWIN* ]]; then
  assert_mode 600 "$PAIR/host.key"
  assert_mode 644 "$PAIR/host.crt"
  assert_mode 644 "$PAIR/host.cert-sha256"
  assert_mode 644 "$PAIR/host.spki-sha256"
  assert_mode 644 "$PAIR/host.generated-by-arcen"
fi
grep -q 'ec_paramgen_curve:P-256' "$OPENSSL_LOG" || fail "P-256 was not requested"
grep -q -- '-sha256' "$OPENSSL_LOG" || fail "SHA-256 was not requested"
grep -q -- '-days 825' "$OPENSSL_LOG" || fail "825-day validity was not requested"
grep -q 'basicConstraints=critical' "$OPENSSL_LOG" &&
  grep -q 'CA:FALSE' "$OPENSSL_LOG" || fail "non-CA leaf constraint missing"
grep -q 'extendedKeyUsage=serverAuth' "$OPENSSL_LOG" || fail "serverAuth EKU missing"
grep -q 'digitalSignature' "$OPENSSL_LOG" || fail "digitalSignature missing"
grep -q 'DNS:pier.example' "$OPENSSL_LOG" || fail "DNS SAN missing"
grep -q 'IP:192.0.2.10' "$OPENSSL_LOG" || fail "IPv4 SAN missing"
grep -q 'IP:2001:db8::10' "$OPENSSL_LOG" || fail "IPv6 SAN missing"

before="$(cksum "$PAIR/host.crt" "$PAIR/host.key" "$PAIR/host.cert-sha256" "$PAIR/host.spki-sha256")"
"$HELPER" --directory "$PAIR"
after="$(cksum "$PAIR/host.crt" "$PAIR/host.key" "$PAIR/host.cert-sha256" "$PAIR/host.spki-sha256")"
[[ "$before" == "$after" ]] || fail "generate-if-missing overwrote a complete pair"

old_key="$(cat "$PAIR/host.key")"
"$HELPER" --renew --directory "$PAIR" --dns pier.example --ip 192.0.2.10
[[ "$(cat "$PAIR/host.key")" == "$old_key" ]] || fail "--renew changed the key"
"$HELPER" --new-key --directory "$PAIR" --dns pier.example --ip 192.0.2.10
[[ "$(cat "$PAIR/host.key")" != "$old_key" ]] || fail "--new-key retained the key"

LEGACY="$WORK/legacy"
"$HELPER" --directory "$LEGACY" --dns pier.example --ip 192.0.2.10
legacy_key="$(cat "$LEGACY/host.key")"
rm "$LEGACY/host.generated-by-arcen"
if "$HELPER" --renew --directory "$LEGACY" --dns pier.example --ip 192.0.2.10; then
  fail "unmarked legacy pair was renewed without explicit adoption"
fi
"$HELPER" --renew --adopt-legacy --directory "$LEGACY" --dns pier.example --ip 192.0.2.10
[[ "$(cat "$LEGACY/host.key")" == "$legacy_key" ]] || fail "legacy adoption changed the key"
[[ -s "$LEGACY/host.generated-by-arcen" ]] || fail "legacy adoption did not write a marker"
if "$HELPER" --adopt-legacy --directory "$LEGACY"; then
  fail "--adopt-legacy was accepted without --renew"
fi

PARTIAL="$WORK/partial"
mkdir -p "$PARTIAL"
printf 'partial\n' >"$PARTIAL/host.key"
if "$HELPER" --directory "$PARTIAL" --dns pier.example --ip 192.0.2.10; then
  fail "partial pair was accepted"
fi

install_failing_mv() {
  cat >"$FAKE_BIN/mv" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == *.stage.*.host.crt && "\${2:-}" == */host.crt ]]; then
  exit 73
fi
exec "$REAL_MV" "\$@"
EOF
  chmod +x "$FAKE_BIN/mv"
}

EMPTY_ROLLBACK="$WORK/empty-rollback"
install_failing_mv
if "$HELPER" --directory "$EMPTY_ROLLBACK" --dns pier.example --ip 192.0.2.10; then
  fail "first-issue publish failure unexpectedly succeeded"
fi
[[ ! -e "$EMPTY_ROLLBACK/host.key" && ! -e "$EMPTY_ROLLBACK/host.crt" ]] ||
  fail "first-issue rollback left a partial pair"
[[ ! -e "$EMPTY_ROLLBACK/.arcen-cert.transaction" ]] ||
  fail "first-issue rollback left a journal"
rm -f "$FAKE_BIN/mv"

ROLLBACK="$WORK/rollback"
"$HELPER" --directory "$ROLLBACK" --dns pier.example --ip 192.0.2.10
rollback_key="$(cat "$ROLLBACK/host.key")"
rollback_cert="$(cat "$ROLLBACK/host.crt")"
install_failing_mv
if PATH="$FAKE_BIN:$PATH" "$HELPER" --renew --directory "$ROLLBACK" --dns pier.example --ip 192.0.2.10; then
  fail "injected publish failure unexpectedly succeeded"
fi
[[ "$(cat "$ROLLBACK/host.key")" == "$rollback_key" ]] || fail "rollback did not restore key"
[[ "$(cat "$ROLLBACK/host.crt")" == "$rollback_cert" ]] || fail "rollback did not restore cert"
rm -f "$FAKE_BIN/mv"

INTERRUPTED="$WORK/interrupted"
"$HELPER" --directory "$INTERRUPTED" --dns pier.example --ip 192.0.2.10
interrupted_key="$(cat "$INTERRUPTED/host.key")"
interrupted_cert="$(cat "$INTERRUPTED/host.crt")"
transaction="999-999"
"$REAL_MV" "$INTERRUPTED/host.key" "$INTERRUPTED/.arcen-cert.backup.$transaction.host.key"
"$REAL_MV" "$INTERRUPTED/host.crt" "$INTERRUPTED/.arcen-cert.backup.$transaction.host.crt"
printf 'new-partial-key\n' >"$INTERRUPTED/host.key"
{
  printf 'transaction=%s\n' "$transaction"
  printf 'phase=prepared\n'
  printf 'existed.host.key=1\n'
  printf 'existed.host.crt=1\n'
  printf 'existed.host.cert-sha256=1\n'
  printf 'existed.host.spki-sha256=1\n'
  printf 'existed.host.generated-by-arcen=1\n'
} >"$INTERRUPTED/.arcen-cert.transaction"
chmod 600 "$INTERRUPTED/.arcen-cert.transaction"
"$HELPER" --directory "$INTERRUPTED"
[[ "$(cat "$INTERRUPTED/host.key")" == "$interrupted_key" ]] || fail "recovery did not restore key"
[[ "$(cat "$INTERRUPTED/host.crt")" == "$interrupted_cert" ]] || fail "recovery did not restore cert"

STALE_MARKER="$WORK/stale-marker"
"$HELPER" --directory "$STALE_MARKER" --dns pier.example --ip 192.0.2.10
sed -i 's/^certificate=.*/certificate=stale/' "$STALE_MARKER/host.generated-by-arcen"
if "$HELPER" --renew --directory "$STALE_MARKER" --dns pier.example --ip 192.0.2.10; then
  fail "stale helper marker authorized custom material overwrite"
fi

COMMITTED="$WORK/committed"
"$HELPER" --directory "$COMMITTED" --dns pier.example --ip 192.0.2.10
transaction="777-777"
for name in host.key host.crt host.cert-sha256 host.spki-sha256 host.generated-by-arcen; do
  cp "$COMMITTED/$name" "$COMMITTED/.arcen-cert.backup.$transaction.$name"
done
printf 'committed-new-cert\n' >"$COMMITTED/host.crt"
{
  printf 'transaction=%s\n' "$transaction"
  printf 'phase=committed\n'
  for name in host.key host.crt host.cert-sha256 host.spki-sha256 host.generated-by-arcen; do
    printf 'existed.%s=1\n' "$name"
  done
} >"$COMMITTED/.arcen-cert.transaction"
chmod 600 "$COMMITTED/.arcen-cert.transaction"
"$HELPER" --directory "$COMMITTED"
[[ "$(cat "$COMMITTED/host.crt")" == "committed-new-cert" ]] ||
  fail "committed recovery mixed in an old generation"
[[ ! -e "$COMMITTED/.arcen-cert.backup.$transaction.host.crt" ]] ||
  fail "committed recovery retained backups"

if [[ "$PLATFORM" == MINGW* || "$PLATFORM" == CYGWIN* ]]; then
  if ARCEN_TEST_FLOCK_FAIL=1 "$HELPER" --directory "$PAIR"; then
    fail "concurrent helper invocation was accepted"
  fi
else
  exec {HELD_LOCK}<>"$PAIR/.arcen-cert.lock"
  flock -n "$HELD_LOCK" || fail "could not establish lock fixture"
  if "$HELPER" --directory "$PAIR"; then
    fail "concurrent helper invocation was accepted"
  fi
  flock -u "$HELD_LOCK"
  exec {HELD_LOCK}>&-
fi

SYMLINK_TARGET="$WORK/symlink-target"
SYMLINK_PAIR="$WORK/symlink-pair"
mkdir -p "$SYMLINK_TARGET" "$SYMLINK_PAIR"
printf 'target\n' >"$SYMLINK_TARGET/key"
ln -s "$SYMLINK_TARGET/key" "$SYMLINK_PAIR/host.key"
if [[ -L "$SYMLINK_PAIR/host.key" ]]; then
  if "$HELPER" --directory "$SYMLINK_PAIR" --dns pier.example --ip 192.0.2.10; then
    fail "symlink destination was accepted"
  fi
fi
ln -s "$SYMLINK_TARGET" "$WORK/symlink-component"
if [[ -L "$WORK/symlink-component" ]]; then
  if "$HELPER" --directory "$WORK/symlink-component/tls" --dns pier.example --ip 192.0.2.10; then
    fail "symlink path component was accepted"
  fi
else
  echo "symlink fixture skipped: platform did not create symbolic links"
fi

if [[ -n "$REAL_OPENSSL" && "$PLATFORM" != MINGW* && "$PLATFORM" != CYGWIN* \
  && "${ARCEN_SKIP_REAL_OPENSSL_SMOKE:-0}" != "1" ]]; then
  REAL_PAIR="$WORK/real"
  OPENSSL="$REAL_OPENSSL" "$HELPER" --directory "$REAL_PAIR" \
    --dns pier-smoke.example --ip 192.0.2.10
  "$REAL_OPENSSL" x509 -in "$REAL_PAIR/host.crt" -noout -checkend 1 >/dev/null
  "$REAL_OPENSSL" x509 -in "$REAL_PAIR/host.crt" -noout -text |
    grep -A1 'X509v3 Basic Constraints: critical' |
    grep -q 'CA:FALSE' || fail "real certificate is not a non-CA leaf"
fi

echo "new-host-cert fixtures passed"
