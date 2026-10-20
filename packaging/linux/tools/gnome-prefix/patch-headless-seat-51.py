#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-only
# Lab only, applied on top of GNOME/mutter!5224 in Mutter 51.1:
# a headless compositor starts libinput only when ARCEN_LAB_HEADLESS_INPUT_SEAT names the
# udev seat (ID_SEAT) to use; unset keeps stock headless behaviour (no libinput at all).
import sys
p = sys.argv[1]
s = open(p).read()
def rep(old, new):
    global s
    assert s.count(old) == 1, "context not found: " + old
    s = s.replace(old, new)
rep("  if (libinput_udev_assign_seat (libinput, udev_seat_id) == -1)",
    "  if ((seat_impl->flags & META_SEAT_NATIVE_FLAG_NO_LIBINPUT) &&\n"
    "      g_getenv (\"ARCEN_LAB_HEADLESS_INPUT_SEAT\"))\n"
    "    udev_seat_id = g_getenv (\"ARCEN_LAB_HEADLESS_INPUT_SEAT\");\n\n"
    "  if (libinput_udev_assign_seat (libinput, udev_seat_id) == -1)")
rep("  if (!init_libinput (seat_impl, &error))",
    "  if ((!(seat_impl->flags & META_SEAT_NATIVE_FLAG_NO_LIBINPUT) ||\n"
    "       g_getenv (\"ARCEN_LAB_HEADLESS_INPUT_SEAT\")) &&\n"
    "      !init_libinput (seat_impl, &error))")
# mutter!5224 still asks logind to TakeDevice while libinput enumerates at
# start (seat_impl->libinput is not set yet), which a seatless headless session
# cannot: a headless compositor always opens its devices directly.
rep("""  if (!g_str_has_prefix (path, "/sys/") &&
      ((seat_impl->libinput &&
        !(seat_impl->flags & META_SEAT_NATIVE_FLAG_NO_LIBINPUT)) ||
      !seat_impl->libinput))""",
    """  if (!g_str_has_prefix (path, "/sys/") &&
      !(seat_impl->flags & META_SEAT_NATIVE_FLAG_NO_LIBINPUT))""")

# The directly opened device (no logind TakeDevice) comes from a plain
# open (path, O_RDWR): libinput asks for O_NONBLOCK, and without it the input
# thread blocks in read() until the device produces an event; without
# FD_CLOEXEC every program the compositor starts inherits the device.
rep("  fd = meta_device_file_get_fd (device_file);\n",
    "  fd = meta_device_file_get_fd (device_file);\n"
    "  if (!(flags & META_DEVICE_FILE_FLAG_TAKE_CONTROL))\n"
    "    {\n"
    "      fcntl (fd, F_SETFL, fcntl (fd, F_GETFL) | O_NONBLOCK);\n"
    "      fcntl (fd, F_SETFD, FD_CLOEXEC);\n"
    "    }\n")
open(p, "w").write(s)
print("patched", p)
