#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-only
# Lab-only edit of mutter 49.4 src/backends/native/meta-seat-impl.c, after
# GNOME/mutter!5224 ("seat/native: Enable libinput in headless"), plus an
# explicit seat: only when ARCEN_LAB_HEADLESS_INPUT_SEAT is set does a headless
# compositor start libinput, on that udev seat (ID_SEAT), opening device nodes
# directly (no logind TakeDevice). Unset = stock behaviour.
import sys
p=sys.argv[1]; s=open(p).read()
def rep(old,new):
    global s
    assert s.count(old)==1, old
    s=s.replace(old,new)
rep('''  if (!g_str_has_prefix (path, "/sys/"))
    flags |= META_DEVICE_FILE_FLAG_TAKE_CONTROL;''',
'''  if (!g_str_has_prefix (path, "/sys/") &&
      !(seat_impl->flags & META_SEAT_NATIVE_FLAG_NO_LIBINPUT))
    flags |= META_DEVICE_FILE_FLAG_TAKE_CONTROL;''')
rep('''  if (libinput_udev_assign_seat (libinput, seat_impl->seat_id) == -1)''',
'''  if (libinput_udev_assign_seat (libinput,
                                 (seat_impl->flags & META_SEAT_NATIVE_FLAG_NO_LIBINPUT) ?
                                 g_getenv ("ARCEN_LAB_HEADLESS_INPUT_SEAT") :
                                 seat_impl->seat_id) == -1)''')
rep('''  if (!(seat_impl->flags & META_SEAT_NATIVE_FLAG_NO_LIBINPUT))
    {
      g_autoptr (GError) error = NULL;''',
'''  if (!(seat_impl->flags & META_SEAT_NATIVE_FLAG_NO_LIBINPUT) ||
      g_getenv ("ARCEN_LAB_HEADLESS_INPUT_SEAT"))
    {
      g_autoptr (GError) error = NULL;''')
rep("  fd = meta_device_file_get_fd (device_file);\n",
    "  fd = meta_device_file_get_fd (device_file);\n"
    "  if (!(flags & META_DEVICE_FILE_FLAG_TAKE_CONTROL))\n"
    "    {\n"
    "      fcntl (fd, F_SETFL, fcntl (fd, F_GETFL) | O_NONBLOCK);\n"
    "      fcntl (fd, F_SETFD, FD_CLOEXEC);\n"
    "    }\n")
open(p,'w').write(s)
print("patched",p)
