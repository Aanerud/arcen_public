#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY="$SCRIPT_DIRECTORY/deploy-pier.sh"
PREFLIGHT="$SCRIPT_DIRECTORY/deploy-pier-preflight.sh"
WORK="$SCRIPT_DIRECTORY/.deploy-pier-test-$$"
FAKE_BIN="$WORK/bin"
SSH_LOG="$WORK/ssh.log"
SYSTEMCTL_LOG="$WORK/systemctl.log"

cleanup() {
  rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$FAKE_BIN"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

cat >"$FAKE_BIN/rsync" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat >"$FAKE_BIN/ssh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$SSH_LOG"
exit 0
EOF
cat >"$FAKE_BIN/systemctl" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$SYSTEMCTL_LOG"
exit 0
EOF
cat >"$FAKE_BIN/sleep" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat >"$FAKE_BIN/ss" <<'EOF'
#!/usr/bin/env bash
if [[ "$*" == *-lunp* ]]; then
  printf 'UNCONN 0 0 0.0.0.0:18444 0.0.0.0:* users:(("arcen-pier",pid=1,fd=2))\n'
fi
if [[ "${ARCEN_TEST_LEGACY_TCP:-0}" == 1 && "$*" == *-ltnp* ]]; then
  printf 'LISTEN 0 128 0.0.0.0:18443 0.0.0.0:* users:(("arcen-pier",pid=1,fd=1))\n'
fi
EOF
cat >"$FAKE_BIN/arcen-pier" <<'EOF'
#!/usr/bin/env bash
test "$1" = "validate-config"
EOF
chmod +x "$FAKE_BIN"/*
export SSH_LOG SYSTEMCTL_LOG

: >"$SSH_LOG"
PATH="$FAKE_BIN:$PATH" HOST=root@example KEY="$WORK/key" \
  bash "$DEPLOY" >"$WORK/default.out" 2>"$WORK/default.err"
if grep -q -- '--features' "$SSH_LOG"; then
  fail "deployment unexpectedly enabled extra Cargo features"
fi

: >"$SSH_LOG"
if PATH="$FAKE_BIN:$PATH" HOST=root@example KEY="$WORK/key" \
  bash "$DEPLOY" --unknown >"$WORK/unknown.out" 2>"$WORK/unknown.err"; then
  fail "unknown deployment argument was accepted"
fi
grep -q 'unknown argument: --unknown' "$WORK/unknown.err" ||
  fail "unknown deployment argument was not explained"
[[ ! -s "$SSH_LOG" ]] || fail "unknown argument reached SSH"

: >"$SYSTEMCTL_LOG"
PATH="$FAKE_BIN:$PATH" \
  bash "$PREFLIGHT" "$FAKE_BIN/arcen-pier" >"$WORK/valid.out" 2>"$WORK/valid.err"
grep -q '^is-active arcen-pier.service$' "$SYSTEMCTL_LOG" ||
  fail "preflight did not verify service health"

: >"$SYSTEMCTL_LOG"
if ARCEN_TEST_LEGACY_TCP=1 PATH="$FAKE_BIN:$PATH" \
  bash "$PREFLIGHT" "$FAKE_BIN/arcen-pier" \
    >"$WORK/legacy-tcp.out" 2>"$WORK/legacy-tcp.err"; then
  fail "preflight accepted a legacy WSS TCP listener"
fi
grep -q 'legacy WSS TCP port 18443 is still listening' "$WORK/legacy-tcp.err" ||
  fail "legacy WSS listener failure was not explained"

echo "deploy-pier tests passed"
