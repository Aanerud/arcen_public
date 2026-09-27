#!/bin/bash
# Removes Arcen Pier, the way an administrator expects: everything it
# installed, in every session, and then proof that nothing is left running.
#
#   sudo "/Applications/Arcen Pier.app/Contents/Resources/uninstall.sh" [--purge]
#
# By default the host identity, configuration and logs are kept, so a
# reinstall presents the same certificate every Deck already pinned. Deleting
# it is a security decision, made with --purge, which also removes the _arcen
# service account.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=packaging/macos/pier/common.sh
. "$HERE/common.sh"

PURGE=""
for argument in "$@"; do
  case "$argument" in
    --purge) PURGE="yes" ;;
    -h|--help)
      sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown argument: $argument" >&2; exit 2 ;;
  esac
done

if [ "$(id -u)" -ne 0 ]; then
  echo "run with sudo" >&2
  exit 1
fi

arcen_log "stopping the service and every session's agent"
arcen_stop_everything

arcen_log "removing launchd definitions, PAM policy and log rotation"
rm -f "$ARCEN_SERVICE_PLIST" "$ARCEN_AGENT_PLIST" \
  "$ARCEN_LEGACY_SERVICE_PLIST" "$ARCEN_LEGACY_AGENT_PLIST" \
  "$ARCEN_PAM" "$ARCEN_NEWSYSLOG"
rm -f /tmp/arcen-pier.out.log /tmp/arcen-pier.err.log
rm -rf "$ARCEN_RUN"

if /usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate 2>/dev/null | grep -q "enabled"; then
  /usr/libexec/ApplicationFirewall/socketfilterfw --remove "$ARCEN_PIER_BIN" >/dev/null 2>&1 || true
fi

arcen_log "removing the applications"
# This script lives inside the Pier bundle. Removing the bundle while it runs
# is safe: bash already holds the file open.
rm -rf "$ARCEN_PIER_APP" "$ARCEN_AGENT_APP" \
  "$ARCEN_PIER_APP.prev" "$ARCEN_PIER_APP.prev2" \
  "$ARCEN_AGENT_APP.prev" "$ARCEN_AGENT_APP.prev2"
pkgutil --forget pier.arcen.tech >/dev/null 2>&1 || true

if [ -n "$PURGE" ]; then
  arcen_log "purging the host identity, configuration, logs and service account"
  rm -rf "$ARCEN_SUPPORT" "$(dirname "$ARCEN_LOGS")"
  if dscl . -read "/Users/$ARCEN_ACCOUNT" >/dev/null 2>&1; then
    dscl . -delete "/Users/$ARCEN_ACCOUNT" || arcen_log "could not delete user $ARCEN_ACCOUNT" >&2
  fi
  if dscl . -read "/Groups/$ARCEN_ACCOUNT" >/dev/null 2>&1; then
    dscl . -delete "/Groups/$ARCEN_ACCOUNT" || arcen_log "could not delete group $ARCEN_ACCOUNT" >&2
  fi
fi

# --- Check -------------------------------------------------------------------
LEFT=0
if pgrep -f "$(arcen_process_pattern)" >/dev/null 2>&1; then
  arcen_log "still running:" >&2
  pgrep -lf "$(arcen_process_pattern)" >&2
  LEFT=1
fi
if arcen_port_in_use; then
  arcen_log "something is still bound to UDP $ARCEN_PORT:" >&2
  /usr/sbin/lsof -nP -iUDP:"$ARCEN_PORT" >&2
  LEFT=1
fi
for path in "$ARCEN_PIER_APP" "$ARCEN_AGENT_APP" "$ARCEN_SERVICE_PLIST" "$ARCEN_AGENT_PLIST" "$ARCEN_PAM"; do
  if [ -e "$path" ]; then
    arcen_log "not removed: $path" >&2
    LEFT=1
  fi
done
if [ "$LEFT" -ne 0 ]; then
  arcen_log "removal is incomplete" >&2
  exit 1
fi

arcen_log "removed"
if [ -z "$PURGE" ]; then
  echo "Kept: $ARCEN_SUPPORT (host identity and configuration), $ARCEN_LOGS,"
  echo "      and the $ARCEN_ACCOUNT account. Run again with --purge to remove them."
fi
echo "Privacy grants belong to each user; remove them with:"
echo "  tccutil reset All pier.arcen.tech.agent   (as that user)"
exit 0
