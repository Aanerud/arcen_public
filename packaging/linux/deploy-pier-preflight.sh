#!/usr/bin/env bash
set -euo pipefail

if (($# != 1)); then
  echo "usage: deploy-pier-preflight.sh <arcen-pier-binary>" >&2
  exit 2
fi

PIER_BIN="$1"
"$PIER_BIN" validate-config --config /etc/arcen/pier.json >/dev/null
systemctl is-active arcen-pier.service
ss -lunp | grep :18444
if ss -ltnp | grep -q :18443; then
  echo "legacy WSS TCP port 18443 is still listening; QUIC-only deployment failed" >&2
  exit 1
fi
