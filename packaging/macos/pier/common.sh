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
ARCEN_CONFIG="$ARCEN_SUPPORT/pier.json"
ARCEN_LOGS="/Library/Logs/Arcen/Pier"
ARCEN_PAM="/etc/pam.d/arcen"
ARCEN_NEWSYSLOG="/etc/newsyslog.d/pier.arcen.tech.conf"
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

# Stops every Arcen job in every domain, current and legacy.
arcen_stop_everything() {
  local uid
  for uid in $(arcen_gui_uids); do
    arcen_bootout "gui/$uid/$ARCEN_AGENT_LABEL"
    arcen_bootout "gui/$uid/$ARCEN_LEGACY_AGENT_LABEL"
  done
  arcen_bootout "system/$ARCEN_SERVICE_LABEL"
  arcen_bootout "system/$ARCEN_LEGACY_SERVICE_LABEL"
  # A process launchd no longer supervises is still a process. Wait briefly,
  # then make sure: an old agent holding UDP 18444 is what made every new one
  # crash-loop on bind.
  local _
  for _ in 1 2 3 4 5; do
    pgrep -f "$(arcen_process_pattern)" >/dev/null 2>&1 || return 0
    sleep 1
  done
  pkill -f "$(arcen_process_pattern)" >/dev/null 2>&1 || true
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
