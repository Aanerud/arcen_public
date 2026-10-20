#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Core-protocol X11 test client for the headless session lab harness.

The plain-Xlib counterpart of ``wayland-input-client.py`` for X11
applications that use no toolkit frame synchronisation (Motif, Xt, many
OpenGL and Java tools): libX11 through ``ctypes``, nothing else. It opens one
fullscreen window on ``$DISPLAY``, animates it with ``XFillRectangle`` every
``ARCEN_LAB_ANIMATE_MS`` (default 50 ms; SIGUSR1 stops or restarts it),
prints ``MAPPED`` once it is fullscreen and focused, then one line per
pointer, button, scroll and key event, in the same format as the GTK client
so the harness asserts the same things. ``FRAMES n`` reports how many fills
it flushed in each second. Used only by
``hosts/linux/examples/wayland_session_harness.rs`` (``--x11-client``).
"""

import ctypes
import os
import select
import signal
import sys
import time

KEY_PRESS, KEY_RELEASE, BUTTON_PRESS, BUTTON_RELEASE, MOTION = 2, 3, 4, 5, 6
ENTER, LEAVE, FOCUS_IN, FOCUS_OUT, CONFIGURE = 7, 8, 9, 10, 22
MASK = (
    (1 << 0)  # KeyPress
    | (1 << 1)  # KeyRelease
    | (1 << 2)  # ButtonPress
    | (1 << 3)  # ButtonRelease
    | (1 << 4)  # EnterWindow
    | (1 << 5)  # LeaveWindow
    | (1 << 6)  # PointerMotion
    | (1 << 15)  # Exposure
    | (1 << 17)  # StructureNotify
    | (1 << 21)  # FocusChange
)
SCROLL = {4: "up", 5: "down", 6: "left", 7: "right"}


class XEvent(ctypes.Union):
    _fields_ = [("type", ctypes.c_int), ("pad", ctypes.c_long * 24)]


def emit(text):
    print(text, flush=True)


def field(event, offset, kind=ctypes.c_int):
    return kind.from_buffer(event, offset).value


def toplevels(x11, dpy, root):
    """The root's mapped children with their geometry: which X11 windows
    could hold the pointer."""
    ulong = ctypes.c_ulong
    x11.XQueryTree.argtypes = [
        ctypes.c_void_p,
        ulong,
        ctypes.POINTER(ulong),
        ctypes.POINTER(ulong),
        ctypes.POINTER(ctypes.POINTER(ulong)),
        ctypes.POINTER(ctypes.c_uint),
    ]
    x11.XGetWindowAttributes.argtypes = [ctypes.c_void_p, ulong, ctypes.c_char_p]
    root_return, parent = ulong(), ulong()
    children = ctypes.POINTER(ulong)()
    count = ctypes.c_uint()
    if not x11.XQueryTree(
        dpy,
        root,
        ctypes.byref(root_return),
        ctypes.byref(parent),
        ctypes.byref(children),
        ctypes.byref(count),
    ):
        return []
    lines = []
    attributes = ctypes.create_string_buffer(256)
    for index in range(count.value):
        child = children[index]
        if not x11.XGetWindowAttributes(dpy, child, attributes):
            continue
        x, y, w, h = (ctypes.c_int.from_buffer(attributes, offset).value for offset in (0, 4, 8, 12))
        override = ctypes.c_int.from_buffer(attributes, 120).value
        mapped = ctypes.c_int.from_buffer(attributes, 92).value
        if mapped == 2:
            lines.append(f"TOPLEVEL {child:#x} {w}x{h}+{x}+{y} override_redirect={override}")
    return lines


def main():
    x11 = ctypes.CDLL("libX11.so.6")
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    display = x11.XOpenDisplay(None)
    if not display:
        emit("ERROR cannot open display")
        return 1
    dpy = ctypes.c_void_p(display)
    ulong = ctypes.c_ulong
    x11.XDefaultScreen.argtypes = [ctypes.c_void_p]
    x11.XRootWindow.argtypes = [ctypes.c_void_p, ctypes.c_int]
    x11.XRootWindow.restype = ulong
    x11.XDisplayWidth.argtypes = [ctypes.c_void_p, ctypes.c_int]
    x11.XDisplayHeight.argtypes = [ctypes.c_void_p, ctypes.c_int]
    x11.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ulong] + [ctypes.c_int] * 4 + [
        ctypes.c_uint,
        ulong,
        ulong,
    ]
    x11.XCreateSimpleWindow.restype = ulong
    x11.XSelectInput.argtypes = [ctypes.c_void_p, ulong, ctypes.c_long]
    x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
    x11.XInternAtom.restype = ulong
    x11.XChangeProperty.argtypes = [
        ctypes.c_void_p,
        ulong,
        ulong,
        ulong,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_void_p,
        ctypes.c_int,
    ]
    x11.XStoreName.argtypes = [ctypes.c_void_p, ulong, ctypes.c_char_p]
    x11.XMapWindow.argtypes = [ctypes.c_void_p, ulong]
    x11.XCreateGC.argtypes = [ctypes.c_void_p, ulong, ulong, ctypes.c_void_p]
    x11.XCreateGC.restype = ctypes.c_void_p
    x11.XSetForeground.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ulong]
    x11.XFillRectangle.argtypes = [
        ctypes.c_void_p,
        ulong,
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_uint,
        ctypes.c_uint,
    ]
    x11.XPending.argtypes = [ctypes.c_void_p]
    x11.XNextEvent.argtypes = [ctypes.c_void_p, ctypes.POINTER(XEvent)]
    x11.XConnectionNumber.argtypes = [ctypes.c_void_p]
    x11.XFlush.argtypes = [ctypes.c_void_p]
    x11.XQueryPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong] + [
        ctypes.POINTER(ctypes.c_ulong)
    ] * 2 + [ctypes.POINTER(ctypes.c_int)] * 4 + [ctypes.POINTER(ctypes.c_uint)]
    x11.XLookupString.argtypes = [
        ctypes.POINTER(XEvent),
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.POINTER(ulong),
        ctypes.c_void_p,
    ]

    screen = x11.XDefaultScreen(dpy)
    root = x11.XRootWindow(dpy, screen)
    width = x11.XDisplayWidth(dpy, screen)
    height = x11.XDisplayHeight(dpy, screen)
    window = x11.XCreateSimpleWindow(dpy, root, 0, 0, 800, 600, 0, 0, 0)
    x11.XSelectInput(dpy, window, MASK)
    x11.XStoreName(dpy, window, b"arcen-x11-input-client")
    state = x11.XInternAtom(dpy, b"_NET_WM_STATE", 0)
    fullscreen = (ulong * 1)(x11.XInternAtom(dpy, b"_NET_WM_STATE_FULLSCREEN", 0))
    atom = x11.XInternAtom(dpy, b"ATOM", 0)
    x11.XChangeProperty(dpy, window, state, atom, 32, 0, fullscreen, 1)
    if "cursor" in os.path.basename(sys.argv[0]) or os.environ.get("ARCEN_X11_CLIENT_CURSOR"):
        # A defined cursor (XC_left_ptr), as most toolkits set; without one
        # the window inherits the root's.
        x11.XCreateFontCursor.argtypes = [ctypes.c_void_p, ctypes.c_uint]
        x11.XCreateFontCursor.restype = ulong
        x11.XDefineCursor.argtypes = [ctypes.c_void_p, ulong, ulong]
        x11.XDefineCursor(dpy, window, x11.XCreateFontCursor(dpy, 68))
        emit("CURSOR defined")
    x11.XMapWindow(dpy, window)
    gc = x11.XCreateGC(dpy, window, 0, None)
    x11.XFlush(dpy)
    name = os.environ.get("DISPLAY", "")

    animating = [True]
    signal.signal(
        signal.SIGUSR1,
        lambda *_: (
            animating.__setitem__(0, not animating[0]),
            emit(f"animation {'on' if animating[0] else 'off'}"),
        ),
    )
    period = int(os.environ.get("ARCEN_LAB_ANIMATE_MS", "50")) / 1000.0
    fd = x11.XConnectionNumber(dpy)
    event = XEvent()
    focused = sized = announced = False
    announce_at = None
    frame = 0
    window_size = (800, 600)
    next_draw = time.monotonic()
    second, fills = int(time.monotonic()), 0
    buffer = ctypes.create_string_buffer(32)
    keysym = ulong()
    while True:
        now = time.monotonic()
        if animating[0] and now >= next_draw:
            phase = (frame % 60) / 60.0
            colour = (int(phase * 255) << 16) | (77 << 8) | int((1.0 - phase) * 255)
            x11.XSetForeground(dpy, gc, colour)
            x11.XFillRectangle(dpy, window, gc, 0, 0, window_size[0], window_size[1])
            x11.XFlush(dpy)
            frame += 1
            fills += 1
            next_draw = now + period
        if int(now) != second:
            if animating[0] and announced:
                emit(f"FRAMES {fills}")
            if announced:
                root_x, root_y, win_x, win_y = (ctypes.c_int() for _ in range(4))
                mask = ctypes.c_uint()
                child, root_return = ulong(), ulong()
                x11.XQueryPointer(
                    dpy,
                    window,
                    ctypes.byref(root_return),
                    ctypes.byref(child),
                    ctypes.byref(root_x),
                    ctypes.byref(root_y),
                    ctypes.byref(win_x),
                    ctypes.byref(win_y),
                    ctypes.byref(mask),
                )
                emit(f"POINTER root={root_x.value},{root_y.value}")
            second, fills = int(now), 0
        if announce_at is not None and now >= announce_at and not announced:
            announced = True
            emit(f"MAPPED backend={name}")
            emit(f"SCREEN {width}x{height} window={window_size[0]}x{window_size[1]}")
            for line in toplevels(x11, dpy, root):
                emit(line)
        timeout = max(0.0, min(next_draw - time.monotonic(), 0.05))
        if not x11.XPending(dpy):
            try:
                select.select([fd], [], [], timeout)
            except InterruptedError:
                pass
        while x11.XPending(dpy):
            x11.XNextEvent(dpy, ctypes.byref(event))
            kind = event.type
            if kind in (KEY_PRESS, KEY_RELEASE):
                x11.XLookupString(ctypes.byref(event), buffer, 32, ctypes.byref(keysym), None)
                emit(
                    f"{'key-press' if kind == KEY_PRESS else 'key-release'} "
                    f"keyval={keysym.value:#x} hw={field(event, 84, ctypes.c_uint)} "
                    f"t={field(event, 56, ulong)} device=core"
                )
            elif kind in (BUTTON_PRESS, BUTTON_RELEASE):
                button = field(event, 84, ctypes.c_uint)
                if button in SCROLL:
                    if kind == BUTTON_PRESS:
                        emit(f"scroll direction={SCROLL[button]} device=core")
                else:
                    emit(
                        f"{'button-press' if kind == BUTTON_PRESS else 'button-release'} "
                        f"button={button} t={field(event, 56, ulong)} "
                        f"x={field(event, 64)} y={field(event, 68)} device=core"
                    )
            elif kind == MOTION:
                emit(
                    f"motion x={field(event, 64)}.0 y={field(event, 68)}.0 "
                    f"root={field(event, 72)},{field(event, 76)} device=core"
                )
            elif kind in (ENTER, LEAVE):
                emit(
                    f"{'enter' if kind == ENTER else 'leave'} x={field(event, 64)} "
                    f"y={field(event, 68)} root={field(event, 72)},{field(event, 76)}"
                )
            elif kind in (FOCUS_IN, FOCUS_OUT):
                emit(f"focus in={kind == FOCUS_IN}")
                focused = kind == FOCUS_IN
            elif kind == CONFIGURE:
                window_size = (field(event, 56), field(event, 60))
                sized = window_size[0] >= width and window_size[1] >= height
            if focused and sized and announce_at is None:
                announce_at = time.monotonic() + 0.5


if __name__ == "__main__":
    sys.exit(main())
