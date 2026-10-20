#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""GTK 3 probe for X11 applications in the headless session lab harness.

``set TEXT`` opens a small window, waits until it has focus, owns the
CLIPBOARD with TEXT, prints ``SET`` and keeps owning it until it is killed.
``get`` opens a small window, waits until it has focus, reads the CLIPBOARD
as text, prints ``GOT <text>`` (or ``GOT-NONE``) and exits.
``frames`` opens a fullscreen window that redraws on every frame-clock tick
and prints ``FRAMES n`` for each of four seconds, then exits.

Run with ``GDK_BACKEND=x11`` (through the compositor's Xwayland) or
``GDK_BACKEND=wayland`` to compare the two and to prove the compositor's
X11↔Wayland selection bridge. Used only by
``hosts/linux/examples/wayland_session_harness.rs`` (``--x11-probe``).
"""

import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402


def emit(text):
    print(text, flush=True)


def frames():
    window = Gtk.Window(title="arcen frame probe")
    area = Gtk.DrawingArea()
    window.add(area)
    ticks = [0]
    seconds = []
    started = []

    def on_draw(widget, context):
        phase = (ticks[0] % 60) / 60.0
        context.set_source_rgb(phase, 0.3, 1.0 - phase)
        allocation = widget.get_allocation()
        context.rectangle(0, 0, allocation.width, allocation.height)
        context.fill()
        return False

    def tick(widget, clock):
        ticks[0] += 1
        widget.queue_draw()
        now = clock.get_frame_time()
        if not started:
            started.append(now)
            seconds.append(0)
        if now - started[0] >= 1_000_000 * len(seconds):
            emit(f"FRAMES {seconds[-1]}")
            seconds.append(0)
            if len(seconds) > 4:
                Gtk.main_quit()
                return False
        seconds[-1] += 1
        return True

    area.connect("draw", on_draw)
    area.add_tick_callback(tick)
    window.connect("destroy", Gtk.main_quit)
    window.fullscreen()
    window.show_all()
    GLib.timeout_add(10_000, Gtk.main_quit)
    Gtk.main()
    return 0


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("set", "get", "frames"):
        emit("usage: wayland-x11-probe.py set TEXT | get | frames")
        return 2
    mode = sys.argv[1]
    if mode == "frames":
        return frames()
    text = sys.argv[2] if mode == "set" and len(sys.argv) > 2 else ""
    window = Gtk.Window(title=f"arcen clipboard probe {mode}")
    window.set_default_size(320, 200)
    clipboard = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
    done = []

    def act():
        if done:
            return False
        done.append(True)
        if mode == "set":
            clipboard.set_text(text, -1)
            clipboard.store()
            emit(f"SET backend={Gdk.Display.get_default().get_name()}")
        else:
            got = clipboard.wait_for_text()
            emit(f"GOT {got}" if got is not None else "GOT-NONE")
            Gtk.main_quit()
        return False

    def on_state(_widget, event):
        if event.new_window_state & Gdk.WindowState.FOCUSED:
            GLib.timeout_add(300, act)
        return False

    window.connect("window-state-event", on_state)
    window.connect("destroy", Gtk.main_quit)
    window.show_all()
    window.present()
    # A window that never gets focus still acts, so the harness sees why.
    GLib.timeout_add(8000, act)
    Gtk.main()
    return 0


if __name__ == "__main__":
    sys.exit(main())
