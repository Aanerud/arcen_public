#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
# Host side of the real-screen harness (run as root on the Pier host).
#
#   host_page.sh start <user> <run-dir> [port] [extra-query] [browser] [url]
#   host_page.sh stop  <run-dir>
#   host_page.sh env   <user>
#
# start: waits for <user>'s graphical session (X11 or Wayland, whichever
# Pier created it), starts page_server.py as that user on 127.0.0.1:<port>
# and opens latency.html (or <url>) in a kiosk browser inside that session.
# <browser>: firefox (default; Wayland-native in a Wayland session),
# firefox-x11 (Xwayland), chrome (Chrome's own platform default),
# chrome-wayland, chrome-x11. <run-dir>/browser.txt records the version and
# the display connection the browser actually made. Files live next to this
# script; logs go to <run-dir> (owned by <user>).
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
CMD=${1:-}

session_env() {
  # Prints KEY=VALUE lines for the newest graphical session of $1.
  local user=$1 uid pid
  uid=$(id -u "$user") || return 1
  local env rt
  for pid in $(pgrep -n -u "$uid" -x gnome-shell) $(pgrep -u "$uid" -f gnome-session | sort -rn); do
    [ -r /proc/$pid/environ ] || continue
    env=$(tr '\0' '\n' < /proc/$pid/environ)
    if grep -qE '^(DISPLAY|WAYLAND_DISPLAY)=' <<<"$env"; then
      grep -E '^(DISPLAY|WAYLAND_DISPLAY|XAUTHORITY|DBUS_SESSION_BUS_ADDRESS|XDG_RUNTIME_DIR|XDG_SESSION_TYPE)=' <<<"$env"
      return 0
    fi
    # A headless Arcen Wayland shell keeps its socket and bus in a private
    # runtime directory and does not export WAYLAND_DISPLAY to itself.
    rt=$(sed -n 's/^XDG_RUNTIME_DIR=//p' <<<"$env")
    if [ -n "$rt" ] && [ -S "$rt/wayland-0" ]; then
      echo "WAYLAND_DISPLAY=wayland-0"; echo "XDG_RUNTIME_DIR=$rt"; echo "XDG_SESSION_TYPE=wayland"
      [ -S "$rt/bus" ] && echo "DBUS_SESSION_BUS_ADDRESS=unix:path=$rt/bus"
      return 0
    fi
  done
  # GNOME exports the session environment to the user manager.
  runuser -u "$user" -- env XDG_RUNTIME_DIR=/run/user/$uid systemctl --user show-environment 2>/dev/null \
    | grep -E '^(DISPLAY|WAYLAND_DISPLAY|XAUTHORITY|DBUS_SESSION_BUS_ADDRESS|XDG_RUNTIME_DIR|XDG_SESSION_TYPE)='
}

display_links() {
  # Which display servers the browser's processes are connected to: the
  # compositor's Wayland socket, Xwayland/Xorg's X11 socket, or both.
  python3 - "$1" <<'PY'
import os, re, subprocess, sys
root = int(sys.argv[1]); pids = {root}; grew = True
while grew:
    grew = False
    for d in os.listdir("/proc"):
        if d.isdigit() and int(d) not in pids:
            try:
                pp = int(open(f"/proc/{d}/stat").read().rsplit(")", 1)[1].split()[1])
            except OSError:
                continue
            if pp in pids:
                pids.add(int(d)); grew = True
inodes = set()
for p in pids:
    try:
        for fd in os.listdir(f"/proc/{p}/fd"):
            m = re.match(r"socket:\[(\d+)\]", os.readlink(f"/proc/{p}/fd/{fd}"))
            if m:
                inodes.add(m.group(1))
    except OSError:
        pass
seen = set()
for line in subprocess.run(["ss", "-xa"], capture_output=True, text=True).stdout.splitlines():
    f = line.split()
    if len(f) >= 8 and f[7] in inodes:
        path = f[4]
        if "wayland" in path:
            seen.add("wayland")
        elif "X11-unix" in path or path.startswith("@/tmp/.X11"):
            seen.add("x11:" + path.rsplit("/", 1)[-1])
print(",".join(sorted(seen)) or "unknown")
PY
}

case "$CMD" in
env)
  session_env "$2" ;;
start)
  USER_NAME=$2; RUN=$3; PORT=${4:-18601}; QUERY=${5:-}; BROWSER=${6:-firefox}; URL=${7:-}
  [ -n "$URL" ] || URL="http://127.0.0.1:$PORT/latency.html?$QUERY"
  install -d -m 755 "$RUN"; chown "$USER_NAME" "$RUN"
  ENV=""
  for i in $(seq 1 90); do
    ENV=$(session_env "$USER_NAME")
    echo "$ENV" | grep -qE '^(DISPLAY|WAYLAND_DISPLAY)=' && break
    sleep 1
  done
  [ -n "$ENV" ] || { echo "no graphical session for $USER_NAME" >&2; exit 3; }
  echo "$ENV" | grep -E '^(DISPLAY|WAYLAND_DISPLAY|XDG_SESSION_TYPE)=' > "$RUN/session.env"
  UID_N=$(id -u "$USER_NAME")
  grep -q '^XDG_RUNTIME_DIR=' <<<"$ENV" || ENV="$ENV"$'\n'"XDG_RUNTIME_DIR=/run/user/$UID_N"
  # Fresh profile: no first-run pages, no session restore, no default-browser nag.
  PROFILE="$RUN/ffprofile"; install -d -o "$USER_NAME" -m 700 "$PROFILE"
  cat > "$PROFILE/user.js" <<'JS'
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("startup.homepage_welcome_url", "");
user_pref("startup.homepage_welcome_url.additional", "");
user_pref("browser.aboutwelcome.enabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("dom.event.coalesce_mouse_move", true);
JS
  chown "$USER_NAME" "$PROFILE/user.js"
  : > "$RUN/page.jsonl"; chown "$USER_NAME" "$RUN/page.jsonl"
  runuser -u "$USER_NAME" -- python3 "$HERE/page_server.py" "$PORT" "$RUN/page.jsonl" \
    > "$RUN/server.log" 2>&1 &
  echo $! > "$RUN/server.pid"
  if [ "${BROWSER%-x11}" != "$BROWSER" ] && ! grep -q '^DISPLAY=' <<<"$ENV"; then
    # A Wayland session's Xwayland display is in its clients' environment,
    # not in the compositor's own.
    for pid in $(pgrep -u "$UID_N"); do
      D=$(tr '\0' '\n' < /proc/$pid/environ 2>/dev/null | grep -m1 '^DISPLAY=')
      [ -n "$D" ] && { ENV="$ENV"$'\n'"$D"; break; }
    done
  fi
  case "$BROWSER" in
  firefox|firefox-x11)
    BENV="MOZ_ENABLE_WAYLAND=1"; [ "$BROWSER" = firefox-x11 ] && BENV="MOZ_ENABLE_WAYLAND=0 GDK_BACKEND=x11"
    # shellcheck disable=SC2046,SC2086
    runuser -u "$USER_NAME" -- env $(echo "$ENV" | tr '\n' ' ') $BENV \
      firefox --no-remote --profile "$PROFILE" --kiosk "$URL" > "$RUN/firefox.log" 2>&1 &
    echo $! > "$RUN/firefox.pid"; VERSION=$(firefox --version 2>/dev/null) ;;
  chrome|chrome-wayland|chrome-x11)
    CHROME=$(command -v google-chrome-stable || command -v google-chrome || command -v chromium-browser || command -v chromium)
    [ -n "$CHROME" ] || { echo "no Chrome/Chromium on this host" >&2; exit 5; }
    CPROFILE="$RUN/chprofile"; install -d -o "$USER_NAME" -m 700 "$CPROFILE"
    OZ=""; case "$BROWSER" in chrome-wayland) OZ="--ozone-platform=wayland";; chrome-x11) OZ="--ozone-platform=x11";; esac
    # shellcheck disable=SC2046,SC2086
    runuser -u "$USER_NAME" -- env $(echo "$ENV" | tr '\n' ' ') \
      "$CHROME" --user-data-dir="$CPROFILE" --no-first-run --no-default-browser-check --disable-sync \
      --password-store=basic $OZ --kiosk "$URL" > "$RUN/firefox.log" 2>&1 &
    echo $! > "$RUN/firefox.pid"; VERSION=$("$CHROME" --version 2>/dev/null) ;;
  *) echo "unknown browser $BROWSER" >&2; exit 2 ;;
  esac
  # A reattached headless desktop can come back behind the screen shield;
  # keep the user's sessions unlocked while the page runs.
  ( for i in $(seq 1 900); do
      for s in $(loginctl list-sessions --no-legend | awk -v u="$USER_NAME" '$3==u {print $1}'); do
        [ "$(loginctl show-session "$s" -p LockedHint --value 2>/dev/null)" = yes ] && loginctl unlock-session "$s"
      done
      sleep 2
    done ) > /dev/null 2>&1 &
  echo $! > "$RUN/unlock.pid"
  sleep 6
  echo "$BROWSER $VERSION display=$(display_links "$(cat "$RUN/firefox.pid")")" > "$RUN/browser.txt"
  chown "$USER_NAME" "$RUN/browser.txt"
  case "$URL" in http://127.0.0.1:*) ;; *) sleep 6; echo "PAGE_READY external $(cat "$RUN/browser.txt") $(cat "$RUN/session.env" | tr '\n' ' ')"; exit 0 ;; esac
  for i in $(seq 1 30); do [ -s "$RUN/page.jsonl" ] && break; sleep 1; done
  if [ -s "$RUN/page.jsonl" ]; then echo "PAGE_READY $(cat "$RUN/session.env" | tr '\n' ' ')"; else echo "PAGE_NOT_REPORTING" >&2; exit 4; fi
  ;;
stop)
  RUN=$2
  [ -s "$RUN/unlock.pid" ] && kill "$(cat "$RUN/unlock.pid")" 2>/dev/null
  for f in firefox server; do
    [ -s "$RUN/$f.pid" ] || continue
    P=$(cat "$RUN/$f.pid")
    # runuser forks the real process; kill its children by PID first.
    for c in $(pgrep -P "$P"); do for g in $(pgrep -P "$c"); do kill "$g" 2>/dev/null; done; kill "$c" 2>/dev/null; done
    kill "$P" 2>/dev/null
  done
  sleep 1
  # Firefox content processes are grandchildren of the main process, which we
  # killed above; sweep anything still using this run's profile by PID.
  for p in $(pgrep -f -- "--profile $RUN/ffprofile"); do kill "$p" 2>/dev/null; done
  for p in $(pgrep -f -- "--user-data-dir=$RUN/chprofile"); do kill "$p" 2>/dev/null; done
  echo STOPPED
  ;;
*)
  echo "usage: $0 start <user> <run-dir> [port] [query] | stop <run-dir> | env <user>" >&2; exit 2 ;;
esac
