#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Lab-only Wayland capture harness run. Not shipped.
#
# Run as a disposable, unprivileged user inside its own D-Bus session:
#
#   runuser -u <test-user> -- dbus-run-session -- \
#       scripts/lab/wayland-capture-probe.sh <capenc-binary> [probe args...]
#
# With ARCEN_LAB_CAPENC_RUN=<file>, the arguments are a capenc product argv
# instead (for example `0 h264 30 framed-v1 encoder=nvenc cursor=host`):
# capenc runs its Wayland Auto/Speed pipeline on the hand-off for
# ARCEN_LAB_RUN_SECONDS, writing the framed stream to <file>.
#
# It starts a private PipeWire + WirePlumber (video only), a headless Mutter
# with one virtual monitor, an animated GTK client, and a ScreenCast session
# that records the monitor with cursor metadata. It then runs
# `capenc wayland-capture-probe` with the session hand-off in the
# environment, tears everything down, and removes its runtime directory.
#
# Environment:
#   ARCEN_LAB_MODE        monitor (default) or virtual
#   ARCEN_LAB_SIZE        virtual monitor size, default 1920x1080
#   ARCEN_LAB_REFRESH     virtual monitor refresh in Hz, default the compositor's
#   ARCEN_LAB_COMPOSITOR  mutter (default) or gnome-shell
#   ARCEN_LAB_POINTER_MS  pointer motion period for cursor-only buffers, 0 = off
#   ARCEN_LAB_CLIENT      1 (default) runs the animated client, still a fixed
#                         pattern (for verify-gpu-import), 0 no client
#   ARCEN_LAB_CAST_SECONDS stop the ScreenCast after this long (producer loss test)
#   ARCEN_LAB_CURSOR_MODE ScreenCast cursor-mode: embedded or metadata (default)
#   ARCEN_LAB_CAPENC_RUN  run the product pipeline, framed output to this file
#   ARCEN_LAB_RUN_SECONDS how long the product pipeline runs, default 10
#   ARCEN_CAPTURE_GPU_PCI passed through to capenc (encoder GPU)
set -u

CAPENC=${1:?usage: wayland-capture-probe.sh <capenc> [probe args...]}
shift
HERE=$(cd "$(dirname "$0")" && pwd)
LAB="$HERE/wayland-capture-lab.py"
SIZE=${ARCEN_LAB_SIZE:-1920x1080}
MONITOR=$SIZE
[ -n "${ARCEN_LAB_REFRESH:-}" ] && MONITOR="$SIZE@$ARCEN_LAB_REFRESH"
MODE=${ARCEN_LAB_MODE:-monitor}
POINTER_MS=${ARCEN_LAB_POINTER_MS:-0}
CLIENT=${ARCEN_LAB_CLIENT:-1}
CAST_SECONDS=${ARCEN_LAB_CAST_SECONDS:-600}
CURSOR=${ARCEN_LAB_CURSOR_MODE:-metadata}
case "$CURSOR" in
    embedded) CURSOR_VALUE=1 ;;
    metadata) CURSOR_VALUE=2 ;;
    *) echo "ARCEN_LAB_CURSOR_MODE must be embedded or metadata" >&2; exit 2 ;;
esac

if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    echo "run inside dbus-run-session" >&2
    exit 2
fi

RT="$HOME/arcen-wayland-capture-run"
rm -rf "$RT"
mkdir -m 700 "$RT"
export XDG_RUNTIME_DIR="$RT"
PIDS=()

cleanup() {
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null
    done
    sleep 1
    for pid in "${PIDS[@]}"; do
        kill -9 "$pid" 2>/dev/null
    done
    wait 2>/dev/null
    rm -rf "$RT"
}
trap cleanup EXIT

pipewire > "$RT/pipewire.log" 2>&1 &
PIDS+=($!)
sleep 0.5

# WirePlumber with only the video graph: no ALSA, Bluetooth, V4L2 or camera
# monitors, so the run never touches devices.
WP="$RT/wireplumber"
mkdir -p "$WP"
for src in /usr/share/wireplumber /etc/wireplumber; do
    [ -d "$src" ] || continue
    (cd "$src" && find . -type d | while read -r d; do mkdir -p "$WP/$d"; done
     find . -type f | while read -r f; do ln -sfn "$src/${f#./}" "$WP/$f"; done)
done
mkdir -p "$WP/main.lua.d" "$WP/bluetooth.lua.d"
printf 'alsa_monitor.enabled = false\nv4l2_monitor.enabled = false\nlibcamera_monitor.enabled = false\n' \
    > "$WP/main.lua.d/51-arcen-video-only.lua"
printf 'bluez_monitor.enabled = false\nbluez_midi_monitor.enabled = false\n' \
    > "$WP/bluetooth.lua.d/51-arcen-video-only.lua"
# WirePlumber 0.5 ignores the Lua files above and reads SPA-JSON fragments.
mkdir -p "$WP/wireplumber.conf.d"
printf 'wireplumber.profiles = {\n  main = {\n    monitor.alsa = disabled\n    monitor.alsa-midi = disabled\n    monitor.bluez = disabled\n    monitor.bluez-midi = disabled\n    monitor.libcamera = disabled\n    monitor.v4l2 = disabled\n  }\n}\n' \
    > "$WP/wireplumber.conf.d/99-arcen-video-only.conf"
WIREPLUMBER_CONFIG_DIR="$WP" wireplumber > "$RT/wireplumber.log" 2>&1 &
PIDS+=($!)
sleep 1

"${ARCEN_LAB_COMPOSITOR:-mutter}" --wayland --headless --no-x11 --virtual-monitor "$MONITOR" \
    --wayland-display=wayland-arcen-capture > "$RT/mutter.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do
    gdbus introspect --session -d org.gnome.Mutter.ScreenCast \
        -o /org/gnome/Mutter/ScreenCast > /dev/null 2>&1 && break
    sleep 0.2
done

if [ "$CLIENT" = 1 ] || [ "$CLIENT" = still ]; then
    CLIENT_ARGS=()
    [ "$CLIENT" = still ] && CLIENT_ARGS+=(--still)
    WAYLAND_DISPLAY=wayland-arcen-capture GDK_BACKEND=wayland python3 "$LAB" client \
        "${CLIENT_ARGS[@]}" > "$RT/client.log" 2>&1 &
    PIDS+=($!)
    sleep 1
fi

SC_ARGS=(--seconds "$CAST_SECONDS" --pointer-ms "$POINTER_MS" --node-file "$RT/node"
    --cursor-mode "$CURSOR_VALUE")
[ "$MODE" = virtual ] && SC_ARGS+=(--virtual)
python3 "$LAB" screencast "${SC_ARGS[@]}" > "$RT/screencast.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do
    [ -s "$RT/node" ] && break
    sleep 0.2
done
if [ ! -s "$RT/node" ]; then
    echo "no PipeWire node from ScreenCast" >&2
    cat "$RT/screencast.log" "$RT/mutter.log" >&2
    exit 3
fi

# The shared hand-off contract (arcen_media::video::capture_handoff).
export ARCEN_CAPTURE_PROVIDER=wayland
ARCEN_PIPEWIRE_NODE="$(cat "$RT/node")"
export ARCEN_PIPEWIRE_NODE
export ARCEN_CAPTURE_OUTPUT=Meta-0
export ARCEN_CAPTURE_OUTPUT_GENERATION=1
export ARCEN_CAPTURE_EXPECTED_SIZE="$SIZE"
export ARCEN_CAPTURE_OUTPUT_POSITION=0,0
export ARCEN_CAPTURE_CURSOR_MODE="$CURSOR"
# ARCEN_LAB_PRE_PROBE: probe arguments for a first, short consumer (for
# example `frames=30 max-fps=60`). In virtual mode it creates the monitor at
# its max-fps, so a later consumer of the same size records a monitor whose
# refresh it did not choose.
if [ -n "${ARCEN_LAB_PRE_PROBE:-}" ]; then
    # shellcheck disable=SC2086
    "$CAPENC" wayland-capture-probe $ARCEN_LAB_PRE_PROBE > "$RT/pre-probe.log" 2>&1
    echo "pre-probe exit=$? $(grep -h 'wayland-probe negotiated\|wayland-probe summary' "$RT/pre-probe.log" | tr '\n' ' ')"
    sleep 1
fi
# In virtual mode the client draws on the startup monitor; make the
# recorded virtual monitor the only one so its content is what moves.
if [ "$MODE" = virtual ] && [ -n "${ARCEN_LAB_PRE_PROBE:-}" ]; then
    gdctl set --logical-monitor --primary --monitor Meta-1 > "$RT/gdctl-set.log" 2>&1
    echo "virtual monitor made the only one: exit=$? $(tr '\n' ' ' < "$RT/gdctl-set.log")"
    sleep 1
fi
monitor_modes() {
    gdbus call --session -d org.gnome.Mutter.DisplayConfig -o /org/gnome/Mutter/DisplayConfig \
        -m org.gnome.Mutter.DisplayConfig.GetCurrentState 2>/dev/null |
        grep -o "'[0-9]*x[0-9]*@[0-9.]*', [0-9]*, [0-9]*, [0-9.]*, [0-9.]*, \[[^]]*\], {'is-current': <true>" |
        sed "s/, \[[^]]*\], {'is-current': <true>/ (current)/"
}
echo "monitor modes before: $(monitor_modes | tr '\n' ' ')"
[ -n "${ARCEN_LAB_GDCTL:-}" ] && gdctl show 2>&1 | sed 's/^/gdctl: /' 
if [ -n "${ARCEN_LAB_CAPENC_RUN:-}" ]; then
    # Keep stdin open for the run, then close it: capenc's stop signal.
    sleep "${ARCEN_LAB_RUN_SECONDS:-10}" | "$CAPENC" "$@" > "$ARCEN_LAB_CAPENC_RUN"
    STATUS=$?
    echo "capenc pipeline exit=$STATUS"
else
    "$CAPENC" wayland-capture-probe "$@"
    STATUS=$?
    echo "capenc wayland-capture-probe exit=$STATUS"
fi
echo "monitor modes after: $(monitor_modes | tr '\n' ' ')"
echo "--- mutter log (tail)"
tail -n 8 "$RT/mutter.log"
exit "$STATUS"
