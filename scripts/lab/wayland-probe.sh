#!/usr/bin/env bash
# Read-only Wayland provider qualification probe for a Linux Pier candidate.
#
# Gathers the facts recorded in
# docs/architecture/wayland-qualification/: compositor, PipeWire,
# portal, libei, NVIDIA/DRM, EGL/GBM/CUDA/Vulkan and audio-server state.
#
# It changes nothing: no packages, services, users, sessions, modules or
# configuration are created or modified, and it does not connect to another
# user's session. Live D-Bus checks only read properties, and only when the
# caller already has a Mutter session bus in DBUS_SESSION_BUS_ADDRESS.
#
# Usage: scripts/lab/wayland-probe.sh > wayland-probe.txt
# Running as root adds DRM client details from debugfs when it is mounted.
# Review the output before sharing it: it contains package and driver versions
# but deliberately omits hostnames and addresses.
set -u

section() { printf '\n## %s\n' "$1"; }
fact() { printf '%s: %s\n' "$1" "$2"; }
have() { command -v "$1" >/dev/null 2>&1; }

pkg_version() {
    if have rpm; then
        rpm -q --qf '%{VERSION}-%{RELEASE}\n' "$1" 2>/dev/null | grep -v 'not installed' || echo absent
    elif have dpkg-query; then
        dpkg-query -W -f '${Version}\n' "$1" 2>/dev/null || echo absent
    else
        echo unknown
    fi
}

find_lib() {
    local name=$1 dir
    for dir in /usr/lib64 /usr/lib/x86_64-linux-gnu /usr/lib /lib64; do
        compgen -G "$dir/$name" >/dev/null && { compgen -G "$dir/$name" | head -1; return; }
    done
}

section "System"
fact os "$(. /etc/os-release 2>/dev/null; echo "${PRETTY_NAME:-unknown}")"
fact kernel "$(uname -r)"
fact secure_boot "$(have mokutil && mokutil --sb-state 2>/dev/null | head -1 || echo unknown)"
fact selinux "$(have getenforce && getenforce 2>/dev/null || echo unknown)"

section "Packages"
for p in mutter gnome-shell gnome-remote-desktop pipewire wireplumber pipewire-pulseaudio \
    pipewire-pulse pulseaudio xdg-desktop-portal xdg-desktop-portal-gnome libei libei1 libeis1 \
    xorg-x11-server-Xorg xserver-xorg-core xorg-x11-server-Xwayland xwayland mesa-libgbm libgbm1 \
    libglvnd-egl vulkan-loader gstreamer1 pipewire-gstreamer; do
    v=$(pkg_version "$p")
    [[ $v == absent ]] || fact "$p" "$v"
done
for p in libei libeis; do
    fact "${p}_library" "$(ldconfig -p 2>/dev/null | grep -q "${p}\.so" && echo present || echo absent)"
done

section "Compositor command line"
for c in mutter gnome-shell; do
    if have "$c"; then
        help=$("$c" --help 2>&1)
        for opt in --headless --virtual-monitor --wayland --no-x11; do
            fact "$c $opt" "$(grep -q -- "$opt" <<<"$help" && echo advertised || echo absent)"
        done
    fi
done

section "Mutter D-Bus API (static strings in libmutter)"
libmutter=$(find_lib 'libmutter-*.so.0')
if [[ -n $libmutter ]]; then
    fact libmutter "$libmutter"
    strs=$(strings "$libmutter")
    for s in RecordMonitor RecordVirtual RecordWindow RecordArea ConnectToEIS \
        NotifyPointerMotionAbsolute NotifyKeyboardKeysym NotifyTouchDown NotifyTabletToolMotion \
        EnableClipboard cursor-mode is-platform wp_color_manager_v1 xx_color_manager_v4 \
        zwp_linux_dmabuf_v1 zwp_tablet_manager_v2 bt2100 supported-color-modes; do
        fact "$s" "$(grep -qx -- "$s" <<<"$strs" && echo present || echo absent)"
    done
else
    fact libmutter absent
fi

section "Portal and gnome-remote-desktop"
xml=/usr/share/dbus-1/interfaces/org.freedesktop.portal.RemoteDesktop.xml
if [[ -f $xml ]]; then
    fact portal_remote_desktop_ConnectToEIS "$(grep -q ConnectToEIS "$xml" && echo present || echo absent)"
else
    fact portal_remote_desktop_xml absent
fi
grd=/usr/libexec/gnome-remote-desktop-daemon
[[ -x $grd ]] || grd=$(command -v gnome-remote-desktop-daemon 2>/dev/null || true)
if [[ -n $grd && -x $grd ]]; then
    fact grd_options "$("$grd" --help 2>&1 | grep -oE -- '--[a-z-]+' | grep -v -- '--help' | tr '\n' ' ')"
    fact grd_headless_strings "$(strings "$grd" | grep -ci headless)"
    fact grd_links_freerdp "$(ldd "$grd" 2>/dev/null | grep -qi freerdp && echo yes || echo no)"
fi

section "GPU and DRM"
if have nvidia-smi; then
    fact nvidia_gpu "$(nvidia-smi --query-gpu=name,driver_version,pci.bus_id --format=csv,noheader 2>/dev/null | head -1)"
fi
[[ -r /proc/driver/nvidia/version ]] && fact nvidia_kernel_module "$(head -1 /proc/driver/nvidia/version)"
fact nvidia_modules_loaded "$(awk '$1 ~ /^nvidia/ {print $1}' /proc/modules 2>/dev/null | sort | tr '\n' ' ')"
if have nvidia-smi; then
    # An unlicensed vGPU runs at full capability only for a while after the
    # driver loads, then degrades: rates measured after that are not evidence.
    fact vgpu_license "$(nvidia-smi -q 2>/dev/null | awk '/License Status/ {sub(/^[^:]*: /, ""); print; exit}')"
fi
fact dri_nodes "$(ls /dev/dri 2>/dev/null | tr '\n' ' ')"
for p in modeset fbdev; do
    f=/sys/module/nvidia_drm/parameters/$p
    [[ -r $f ]] && fact "nvidia_drm.$p" "$(cat "$f")"
done
for node in /dev/dri/card* /dev/dri/renderD*; do
    [[ -e $node ]] || continue
    sys=/sys/class/drm/$(basename "$node")
    fact "$node" "$(stat -c '%A %U:%G' "$node") driver=$(basename "$(readlink -f "$sys/device/driver")" 2>/dev/null) pci=$(basename "$(readlink -f "$sys/device")")"
done
connectors=$(compgen -G '/sys/class/drm/card*-*' | wc -l)
fact kms_connectors "$connectors"
if [[ $EUID -eq 0 ]] && compgen -G '/sys/kernel/debug/dri/*/clients' >/dev/null; then
    fact drm_master_holders "$(awk 'NR>1 && $4=="y"{print $1}' /sys/kernel/debug/dri/*/clients 2>/dev/null | sort -u | tr '\n' ' ')"
fi

section "EGL, GBM, CUDA, NVENC"
fact egl_vendors "$(ls /usr/share/glvnd/egl_vendor.d/ 2>/dev/null | tr '\n' ' ')"
fact egl_external_platforms "$(ls /usr/share/egl/egl_external_platform.d/ 2>/dev/null | tr '\n' ' ')"
fact gbm_backends "$(ls /usr/lib64/gbm /usr/lib/x86_64-linux-gnu/gbm 2>/dev/null | tr '\n' ' ')"
for l in libcuda.so.1 libnvidia-encode.so.1 libnvidia-fbc.so.1 libnvidia-egl-gbm.so.1 libEGL_nvidia.so.0; do
    fact "$l" "$(ldconfig -p 2>/dev/null | grep -q "$l" && echo present || echo absent)"
done
cuda=$(find_lib 'libcuda.so.1')
if [[ -n $cuda ]]; then
    for s in cuGraphicsEGLRegisterImage cuGraphicsGLRegisterImage cuImportExternalMemory cuImportExternalSemaphore; do
        fact "libcuda $s" "$(grep -aq "$s" "$cuda" && echo exported || echo absent)"
    done
fi
if have vulkaninfo; then
    vulkaninfo 2>/dev/null | awk '
        /^GPU[0-9]+:/ { gpu=$1 }
        /deviceName/ && gpu { sub(/.*= /, ""); name[gpu]=$0 }
        /VK_EXT_external_memory_dma_buf|VK_EXT_image_drm_format_modifier|VK_KHR_external_semaphore_fd|VK_KHR_video_encode_h265/ && gpu {
            seen[gpu]=seen[gpu] " " $1 }
        END { for (g in name) printf "vulkan %s %s:%s\n", g, name[g], seen[g] }'
fi

section "Audio server (user units, global presets)"
for u in pipewire.socket pipewire-pulse.socket wireplumber.service pulseaudio.socket pulseaudio.service; do
    # is-enabled prints "not-found" and fails for a missing unit; one value only.
    state=$(systemctl --global is-enabled "$u" 2>/dev/null)
    fact "$u" "${state:-absent}"
done

section "Display manager policy"
for f in /etc/gdm/custom.conf /etc/gdm3/custom.conf /etc/gdm3/daemon.conf; do
    [[ -r $f ]] && fact "$f WaylandEnable" "$(grep -E '^\s*WaylandEnable' "$f" | tail -1 | cut -d= -f2 || true)"
done

section "Live Mutter session (read-only, caller's own bus)"
if [[ -n ${DBUS_SESSION_BUS_ADDRESS:-} ]] && have gdbus; then
    # Ask the bus first so an absent name is never auto-started.
    get() {
        gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus \
            -m org.freedesktop.DBus.NameHasOwner "$1" 2>/dev/null | grep -q true || { echo "not running"; return; }
        gdbus call --session -d "$1" -o "$2" -m org.freedesktop.DBus.Properties.Get "$3" "$4" 2>/dev/null || echo unavailable
    }
    fact screencast_version "$(get org.gnome.Mutter.ScreenCast /org/gnome/Mutter/ScreenCast org.gnome.Mutter.ScreenCast Version)"
    fact remote_desktop_version "$(get org.gnome.Mutter.RemoteDesktop /org/gnome/Mutter/RemoteDesktop org.gnome.Mutter.RemoteDesktop Version)"
    fact remote_desktop_device_types "$(get org.gnome.Mutter.RemoteDesktop /org/gnome/Mutter/RemoteDesktop org.gnome.Mutter.RemoteDesktop SupportedDeviceTypes)"
else
    fact live_session "not checked (no session bus)"
fi
