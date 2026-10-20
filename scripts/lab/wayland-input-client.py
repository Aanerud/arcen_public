#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""GTK 3 Wayland test client for the headless session lab harness.

Opens one fullscreen window in the session it is started in, animates it
(GNOME 40 ScreenCast only delivers frames when something changes), prints
``MAPPED`` once it is fullscreen and focused, then prints one line per
pointer, button, scroll and key event it receives, with the device that
delivered it. It reads nothing and writes nothing else. Used only by
``hosts/linux/examples/wayland_session_harness.rs``.

``ARCEN_LAB_ANIMATE_MS`` sets the redraw period (default 50 ms); SIGUSR1
stops or restarts the animation, so a harness can measure an idle desktop.
"""

import os
import signal
import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402


def emit(text):
    print(text, flush=True)


def device_name(event):
    device = event.get_source_device()
    return device.get_name() if device is not None else "unknown"


def on_motion(_widget, event):
    emit(f"motion x={event.x:.1f} y={event.y:.1f} device={device_name(event)}")
    return False


BUTTON_KINDS = {
    Gdk.EventType.BUTTON_PRESS: "button-press",
    Gdk.EventType.BUTTON_RELEASE: "button-release",
    Gdk.EventType._2BUTTON_PRESS: "button-double",
    Gdk.EventType._3BUTTON_PRESS: "button-triple",
}


def on_button(_widget, event):
    kind = BUTTON_KINDS.get(event.type, f"button-{event.type.value_nick}")
    emit(f"{kind} button={event.button} t={event.time} device={device_name(event)}")
    return False


def on_key(_widget, event):
    kind = "key-press" if event.type == Gdk.EventType.KEY_PRESS else "key-release"
    emit(
        f"{kind} keyval={event.keyval:#x} hw={event.hardware_keycode} t={event.time} "
        f"device={device_name(event)}"
    )
    return False


def on_scroll(_widget, event):
    smooth = ""
    if event.direction == Gdk.ScrollDirection.SMOOTH:
        _ok, dx, dy = event.get_scroll_deltas()
        smooth = f" dx={dx:.3f} dy={dy:.3f}"
    emit(f"scroll direction={event.direction.value_nick}{smooth} device={device_name(event)}")
    return False


def on_focus(_widget, event):
    emit(f"focus in={bool(event.in_)}")
    return False


_announced = []


def announce():
    emit(f"MAPPED backend={Gdk.Display.get_default().get_name()}")
    return False


def on_window_state(_widget, event):
    fullscreen = event.new_window_state & Gdk.WindowState.FULLSCREEN
    focused = event.new_window_state & Gdk.WindowState.FOCUSED
    if fullscreen and focused and not _announced:
        _announced.append(True)
        GLib.timeout_add(500, announce)
    return False


_frame = [0]


def on_draw(widget, context):
    phase = (_frame[0] % 60) / 60.0
    context.set_source_rgb(phase, 0.3, 1.0 - phase)
    allocation = widget.get_allocation()
    context.rectangle(0, 0, allocation.width, allocation.height)
    context.fill()
    return False


_animating = [True]


def animate(area):
    if _animating[0]:
        _frame[0] += 1
        area.queue_draw()
    return True


def toggle_animation():
    _animating[0] = not _animating[0]
    emit(f"animation {'on' if _animating[0] else 'off'}")
    return True


def main():
    window = Gtk.Window(title="arcen-wayland-input-client")
    window.set_default_size(800, 600)
    area = Gtk.DrawingArea()
    area.connect("draw", on_draw)
    window.add(area)
    GLib.timeout_add(int(os.environ.get("ARCEN_LAB_ANIMATE_MS", "50")), animate, area)
    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR1, toggle_animation)
    window.add_events(
        Gdk.EventMask.POINTER_MOTION_MASK
        | Gdk.EventMask.BUTTON_PRESS_MASK
        | Gdk.EventMask.BUTTON_RELEASE_MASK
        | Gdk.EventMask.KEY_PRESS_MASK
        | Gdk.EventMask.KEY_RELEASE_MASK
        | Gdk.EventMask.SCROLL_MASK
        | Gdk.EventMask.SMOOTH_SCROLL_MASK
        | Gdk.EventMask.FOCUS_CHANGE_MASK
    )
    window.connect("motion-notify-event", on_motion)
    window.connect("button-press-event", on_button)
    window.connect("button-release-event", on_button)
    window.connect("key-press-event", on_key)
    window.connect("key-release-event", on_key)
    window.connect("scroll-event", on_scroll)
    window.connect("focus-in-event", on_focus)
    window.connect("focus-out-event", on_focus)
    window.connect("window-state-event", on_window_state)
    window.connect("destroy", Gtk.main_quit)
    window.fullscreen()
    window.show_all()
    window.present()
    Gtk.main()
    return 0


if __name__ == "__main__":
    sys.exit(main())
