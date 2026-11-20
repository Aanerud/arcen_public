#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
# The Linux Pier's certificate helper (`arcen-new-host-cert`): a thin front end
# for `arcen-pier new-host-cert`, which holds the only implementation (the
# shared profile, the names a renewal keeps, the journalled replacement and its
# crash recovery, the lock). Nothing is decided here.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: new-host-cert.sh [--renew [--adopt-legacy] | --new-key] [--directory DIR]
                        [--dns NAME]... [--ip ADDRESS]...

Runs `arcen-pier new-host-cert` with these arguments. No issuance mode creates
material only when there is none, keeps it when it is current and renews it
over the existing key when it is due. --renew reissues over the existing key
now (SPKI pins stay valid). --new-key replaces the key (every Deck re-pins).
--adopt-legacy takes over unmarked output of an earlier Arcen helper, keeping
its key; never use it for enterprise/custom PEM. Without --dns, the host's
FQDN is used; without --ip, its addresses.

The directory defaults to $ARCEN_TLS_DIRECTORY, else the Pier's /etc/arcen.
The Pier binary is $ARCEN_PIER_BIN, else arcen-pier beside this script, else
/opt/arcen/bin/arcen-pier.
EOF
}

pier="${ARCEN_PIER_BIN:-}"
if [[ -z "$pier" ]]; then
  here="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
  for candidate in "$here/arcen-pier" /opt/arcen/bin/arcen-pier; do
    if [[ -x "$candidate" ]]; then
      pier="$candidate"
      break
    fi
  done
fi

arguments=()
directory_given=0
for argument in "$@"; do
  case "$argument" in
    -h|--help)
      usage
      exit 0
      ;;
    --directory)
      directory_given=1
      ;;
  esac
  arguments+=("$argument")
done
if ((directory_given == 0)) && [[ -n "${ARCEN_TLS_DIRECTORY:-}" ]]; then
  arguments=(--directory "$ARCEN_TLS_DIRECTORY" ${arguments[@]+"${arguments[@]}"})
fi

[[ -n "$pier" && -x "$pier" ]] || {
  echo "arcen-pier not found; install the Linux Pier or set ARCEN_PIER_BIN" >&2
  exit 1
}
# The `+` form expands an empty list under `set -u` on every bash version.
exec "$pier" new-host-cert ${arguments[@]+"${arguments[@]}"}
