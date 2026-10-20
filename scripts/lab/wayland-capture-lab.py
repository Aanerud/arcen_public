#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Lab-only helpers for the Wayland capture harness. Not shipped.

`client` draws animated content in a GTK 3 Wayland window so a headless
compositor has frames to deliver; `client --still` draws a fixed pattern
(red and blue ramps, a green ramp, a fine checkerboard) once, so two captures
of the same still desktop can be compared byte for byte. `screencast` plays the session adapter's
part for a throwaway compositor: it creates an org.gnome.Mutter.ScreenCast
session (linked to a RemoteDesktop session so it can move the pointer),
records a monitor or a virtual output with cursor metadata, writes the
PipeWire node id to a file, optionally moves the pointer to produce
cursor-only buffers, and stops the sessions when its time is up.

Run only as a disposable user against a compositor that user started; it
never touches another user's session.
"""

import argparse
import math
import sys
import time


def run_client(args):
    import gi

    gi.require_version("Gtk", "3.0")
    from gi.repository import GLib, Gtk

    window = Gtk.Window(title="arcen-wayland-capture-lab")
    window.set_default_size(args.width, args.height)
    area = Gtk.DrawingArea()
    window.add(area)
    started = time.time()

    def draw(_widget, cr):
        t = time.time() - started
        cr.set_source_rgb(0.1, 0.2, 0.6)
        cr.paint()
        cr.set_source_rgb(1.0, 0.5, 0.0)
        cr.rectangle(100 + 600 * (0.5 + 0.5 * math.sin(t)), 300, 120, 120)
        cr.fill()

    def draw_still(widget, cr):
        import cairo

        width = widget.get_allocated_width()
        height = widget.get_allocated_height()
        third = height // 3
        for y0, stops in (
            (0, ((1, 0, 0), (0, 0, 1))),
            (third, ((0, 0, 0), (0, 1, 0))),
        ):
            ramp = cairo.LinearGradient(0, 0, width, 0)
            ramp.add_color_stop_rgb(0, *stops[0])
            ramp.add_color_stop_rgb(1, *stops[1])
            cr.set_source(ramp)
            cr.rectangle(0, y0, width, third)
            cr.fill()
        for y in range(2 * third, height, 4):
            for x in range(0, width, 4):
                if (x // 4 + y // 4) % 2:
                    cr.set_source_rgb((x % 256) / 255, 0.5, (y % 256) / 255)
                    cr.rectangle(x, y, 4, 4)
                    cr.fill()

    area.connect("draw", draw_still if args.still else draw)
    window.connect("destroy", Gtk.main_quit)
    window.show_all()
    if not args.still:
        GLib.timeout_add(16, lambda: (area.queue_draw(), True)[1])
    Gtk.main()


def run_screencast(args):
    from gi.repository import Gio, GLib

    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    sc_name, rd_name = "org.gnome.Mutter.ScreenCast", "org.gnome.Mutter.RemoteDesktop"

    def call(name, path, iface, method, params=None, rtype=None):
        return bus.call_sync(
            name,
            path,
            iface,
            method,
            params,
            GLib.VariantType(rtype) if rtype else None,
            Gio.DBusCallFlags.NONE,
            -1,
            None,
        )

    def prop(name, path, iface, key):
        return call(
            name,
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            GLib.Variant("(ss)", (iface, key)),
            "(v)",
        )[0]

    rd = call(rd_name, "/org/gnome/Mutter/RemoteDesktop", rd_name, "CreateSession", None, "(o)")[0]
    rd_id = prop(rd_name, rd, rd_name + ".Session", "SessionId")
    sc = call(
        sc_name,
        "/org/gnome/Mutter/ScreenCast",
        sc_name,
        "CreateSession",
        GLib.Variant("(a{sv})", ({"remote-desktop-session-id": GLib.Variant("s", rd_id)},)),
        "(o)",
    )[0]
    # cursor-mode 1 composites the cursor, 2 sends it as stream metadata.
    options = {"cursor-mode": GLib.Variant("u", args.cursor_mode)}
    if args.virtual:
        # As the product records them: GNOME treats the monitor as real.
        options["is-platform"] = GLib.Variant("b", True)
        stream = call(
            sc_name, sc, sc_name + ".Session", "RecordVirtual",
            GLib.Variant("(a{sv})", (options,)), "(o)",
        )[0]
    else:
        stream = call(
            sc_name, sc, sc_name + ".Session", "RecordMonitor",
            GLib.Variant("(sa{sv})", (args.monitor, options)), "(o)",
        )[0]

    def on_stream_added(_conn, _sender, _path, _iface, _signal, params):
        with open(args.node_file, "w", encoding="ascii") as handle:
            handle.write("%d\n" % params[0])
        print("PipeWireStreamAdded node=%d" % params[0], flush=True)

    bus.signal_subscribe(
        None, sc_name + ".Stream", "PipeWireStreamAdded", stream, None, 0, on_stream_added
    )
    call(rd_name, rd, rd_name + ".Session", "Start")
    loop = GLib.MainLoop()
    step = [0]

    def move_pointer():
        x = 400.0 + 300.0 * math.sin(step[0] / 10.0)
        y = 500.0 + 200.0 * math.cos(step[0] / 13.0)
        step[0] += 1
        try:
            call(
                rd_name, rd, rd_name + ".Session", "NotifyPointerMotionAbsolute",
                GLib.Variant("(sdd)", (stream, x, y)),
            )
        except GLib.Error as error:
            print("pointer motion failed: %s" % error.message, flush=True)
        return True

    if args.pointer_ms > 0:
        GLib.timeout_add(args.pointer_ms, move_pointer)
    GLib.timeout_add_seconds(args.seconds, loop.quit)
    loop.run()
    call(rd_name, rd, rd_name + ".Session", "Stop")
    print("screencast stopped", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    client = commands.add_parser("client")
    client.add_argument("--width", type=int, default=1600)
    client.add_argument("--height", type=int, default=900)
    client.add_argument("--still", action="store_true")
    screencast = commands.add_parser("screencast")
    screencast.add_argument("--monitor", default="Meta-0")
    screencast.add_argument("--virtual", action="store_true")
    screencast.add_argument("--seconds", type=int, default=60)
    screencast.add_argument("--pointer-ms", type=int, default=0)
    screencast.add_argument("--node-file", required=True)
    screencast.add_argument("--cursor-mode", type=int, choices=(0, 1, 2), default=2)
    args = parser.parse_args()
    if args.command == "client":
        run_client(args)
    else:
        run_screencast(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
