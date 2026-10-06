#!/bin/bash
# Shared by the Pier's preinstall, postinstall and uninstaller.
#
# Sourced, never run. Every name the three scripts agree on lives here once, so
# an uninstaller cannot forget a label an installer added.

ARCEN_ACCOUNT="_arcen"
ARCEN_ACCOUNT_REALNAME="Arcen Pier service"

ARCEN_SERVICE_LABEL="pier.arcen.tech.service"
ARCEN_AGENT_LABEL="pier.arcen.tech.agent"
ARCEN_SERVICE_PLIST="/Library/LaunchDaemons/pier.arcen.tech.service.plist"
ARCEN_LEGACY_TZ_HELPER_LABEL="pier.arcen.tech.timezone-helper"
ARCEN_LEGACY_TZ_HELPER_PLIST="/Library/LaunchDaemons/pier.arcen.tech.timezone-helper.plist"
ARCEN_AGENT_PLIST="/Library/LaunchAgents/pier.arcen.tech.agent.plist"
# What earlier builds installed. Upgrades and uninstalls remove all of it.
ARCEN_LEGACY_AGENT_LABEL="pier.arcen.tech"
ARCEN_LEGACY_AGENT_PLIST="/Library/LaunchAgents/pier.arcen.tech.plist"
ARCEN_LEGACY_SERVICE_LABEL="com.arcen.pier"
ARCEN_LEGACY_SERVICE_PLIST="/Library/LaunchDaemons/com.arcen.pier.plist"
ARCEN_PIER_APP="/Applications/Arcen Pier.app"
# The helper is a background component, so it lives with other privileged
# helpers rather than in /Applications, where it looked like a second app.
ARCEN_AGENT_APP="/Library/PrivilegedHelperTools/Arcen Agent Helper.app"
ARCEN_LEGACY_AGENT_APP="/Applications/Arcen Agent Helper.app"
ARCEN_PIER_BIN="$ARCEN_PIER_APP/Contents/MacOS/arcen-pier-macos"
ARCEN_AGENT_BIN="$ARCEN_AGENT_APP/Contents/MacOS/arcen-agent-helper"

ARCEN_SUPPORT="/Library/Application Support/Arcen"
ARCEN_TLS="$ARCEN_SUPPORT/tls"
ARCEN_RUN="$ARCEN_SUPPORT/run"
ARCEN_RECOVERY="$ARCEN_SUPPORT/recovery"
ARCEN_LEGACY_TZ_JOURNAL="$ARCEN_RECOVERY/timezone-recovery.json"
ARCEN_TZ_SOCKET_DIR="$ARCEN_SUPPORT/timezone"
ARCEN_CONFIG="$ARCEN_SUPPORT/pier.json"
ARCEN_LOGS="/Library/Logs/Arcen/Pier"
ARCEN_PAM="/etc/pam.d/arcen"
ARCEN_NEWSYSLOG="/etc/newsyslog.d/pier.arcen.tech.conf"
ARCEN_MICROPHONE_DRIVER="/Library/Audio/Plug-Ins/HAL/ArcenMicrophone.driver"
ARCEN_PORT=18444
# The bundle identifiers Arcen asks privacy approvals for.
ARCEN_TCC_BUNDLES="pier.arcen.tech pier.arcen.tech.agent"

arcen_log() {
  echo "Arcen Pier: $*"
}

# The uid of every graphical session, the login window's included.
#
# loginwindow runs once per session as that session's user, so this finds
# every user an agent may be running for — not only whoever is at the screen.
# Stopping only the console user's agent is how an earlier uninstaller left a
# listener running in a Fast User Switching session after saying it had
# removed everything.
arcen_gui_uids() {
  ps -axo uid=,comm= | awk '$2 ~ /\/loginwindow$|^loginwindow$/ { print $1 }' | sort -un
}

# Stops a job and never disables it. `launchctl disable` persists in launchd's
# own database, outlives the plist and the uninstall, and makes the next
# install's bootstrap fail with an error that names neither.
arcen_bootout() {
  launchctl bootout "$1" >/dev/null 2>&1 || true
}

arcen_stop_service_and_agents() {
  local uid
  for uid in $(arcen_gui_uids); do
    arcen_bootout "gui/$uid/$ARCEN_AGENT_LABEL"
    arcen_bootout "gui/$uid/$ARCEN_LEGACY_AGENT_LABEL"
  done
  arcen_bootout "system/$ARCEN_SERVICE_LABEL"
  arcen_bootout "system/$ARCEN_LEGACY_SERVICE_LABEL"
}

# Stops every Arcen job in every domain, current and legacy.
arcen_stop_everything() {
  arcen_stop_service_and_agents
  arcen_bootout "system/$ARCEN_LEGACY_TZ_HELPER_LABEL"
  # A process launchd no longer supervises is still a process. Wait briefly,
  # then make sure: an old agent holding UDP 18444 is what made every new one
  # crash-loop on bind.
  local _ pid
  for _ in 1 2 3 4 5; do
    pgrep -f "$(arcen_process_pattern)" >/dev/null 2>&1 || return 0
    sleep 1
  done
  for pid in $(pgrep -f "$(arcen_process_pattern)" 2>/dev/null || true); do
    kill "$pid" >/dev/null 2>&1 || true
  done
  for _ in 1 2 3 4 5; do
    pgrep -f "$(arcen_process_pattern)" >/dev/null 2>&1 || return 0
    sleep 1
  done
  for pid in $(pgrep -f "$(arcen_process_pattern)" 2>/dev/null || true); do
    kill -9 "$pid" >/dev/null 2>&1 || true
  done
  for _ in 1 2 3; do
    pgrep -f "$(arcen_process_pattern)" >/dev/null 2>&1 || return 0
    sleep 1
  done
}

# Matches a process whose program *is* an Arcen binary, not one whose command
# line merely mentions it. Unanchored, `pkill -f` also killed the shell that
# ran the installer over SSH — its own command line named the Pier's path —
# and would take out an administrator's `tail` of a log the same way.
arcen_process_pattern() {
  printf '^(%s|%s)( |$)' "$ARCEN_AGENT_BIN" "$ARCEN_PIER_BIN"
}

# Whether anything is still bound to the Pier's port.
arcen_port_in_use() {
  /usr/sbin/lsof -nP -iUDP:"$ARCEN_PORT" >/dev/null 2>&1
}


arcen_plutil_raw() {
  local key="$1"
  local file="$2"
  /usr/bin/plutil -extract "$key" raw -o - "$file" 2>/dev/null
}

arcen_valid_zone() {
  local zone="$1"
  case "$zone" in
    ""|/*|*..*|*//*|posix/*|*/posix/*|right/*|*/right/*) return 1 ;;
    *[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_+/-]*) return 1 ;;
  esac
  [ -f "/usr/share/zoneinfo/$zone" ]
}

arcen_current_system_timezone() {
  /usr/sbin/systemsetup -gettimezone 2>/dev/null | /usr/bin/sed 's/^Time Zone: //'
}

arcen_stop_legacy_timezone_helper() {
  arcen_bootout "system/$ARCEN_LEGACY_TZ_HELPER_LABEL"
  local _
  for _ in 1 2 3 4 5; do
    if ! pgrep -f "^$ARCEN_PIER_BIN timezone-helper( |$)" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 0
}

arcen_restore_legacy_timezone_journal() {
  [ -f "$ARCEN_LEGACY_TZ_JOURNAL" ] || return 0
  [ ! -L "$ARCEN_LEGACY_TZ_JOURNAL" ] || { arcen_log "refusing symlink legacy timezone journal" >&2; return 1; }
  local original target current
  original="$(arcen_plutil_raw original.iana "$ARCEN_LEGACY_TZ_JOURNAL" 2>/dev/null || true)"
  target="$(arcen_plutil_raw target.iana "$ARCEN_LEGACY_TZ_JOURNAL" 2>/dev/null || true)"
  [ -f "$ARCEN_LEGACY_TZ_JOURNAL" ] || return 0
  arcen_valid_zone "$original" || { arcen_log "legacy timezone journal has no usable original zone" >&2; return 1; }
  current="$(arcen_current_system_timezone || true)"
  if [ -n "$target" ] && [ -n "$current" ] && [ "$current" != "$target" ]; then
    rm -f "$ARCEN_LEGACY_TZ_JOURNAL"
    return 0
  fi
  /usr/sbin/systemsetup -settimezone "$original" >/dev/null || return 1
  rm -f "$ARCEN_LEGACY_TZ_JOURNAL"
}

arcen_microphone_driver_process_running() {
  pgrep -f 'ArcenMicrophone[.]driver' >/dev/null 2>&1
}

arcen_restart_coreaudiod() {
  # Under SIP, launchctl kickstart is not enough to evict AudioServerPlugIn
  # host processes. root may signal coreaudiod; launchd respawns it and reloads
  # the HAL driver set.
  /usr/bin/killall coreaudiod >/dev/null 2>&1 \
    || /bin/launchctl kickstart -k system/com.apple.audio.coreaudiod >/dev/null 2>&1 \
    || true
  local _
  for _ in 1 2 3 4 5; do
    pgrep -x coreaudiod >/dev/null 2>&1 && break
    sleep 1
  done
}
