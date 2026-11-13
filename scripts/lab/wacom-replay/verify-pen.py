#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
"""Assert the emulated Wacom's scripted pen strokes on a Linux host.

Run as root on the Pier host while a Deck plays the stroke with
`arcen-deck input-smoke ... --usb-emulate wacom-intuos5`. It waits for the
kernel's Wacom pen input device (USB 056a:0317, name ending in "Pen") to
appear, opens its event node -- which is what makes usbhid start polling the
pen -- records every event in evtest's format, and checks each stroke against
the script in shared/usb-bridge/src/wacom_intuos5.rs:

  proximity in (Grip Pen 0x802, serial 0x0a2ce017) hovering at (16000, 12000)
  barrel button (BTN_STYLUS) pressed and released while hovering
  pen down at (16000, 12000), pressure ramping 127 .. 2047 in 16 steps
  move to (48000, 12000), then to (48000, 28000), at pressure 1024
  pressure falling to 0 at (48000, 28000), hover, proximity out

This is the kernel-level proof (evdev). Whether the session's display server
uses the device is a separate, X11-level check (see README.md).

Standard library only; no evtest or libinput needed on the host.
"""

import argparse
import fcntl
import json
import os
import select
import struct
import sys
import time

EV_SYN, EV_KEY, EV_ABS, EV_MSC = 0, 1, 3, 4
SYN_REPORT = 0
MSC_SERIAL = 0
KEY_NAMES = {0x140: "BTN_TOOL_PEN", 0x141: "BTN_TOOL_RUBBER", 0x14A: "BTN_TOUCH",
             0x14B: "BTN_STYLUS", 0x14C: "BTN_STYLUS2"}
ABS_NAMES = {0x00: "ABS_X", 0x01: "ABS_Y", 0x05: "ABS_RZ", 0x06: "ABS_THROTTLE",
             0x18: "ABS_PRESSURE", 0x19: "ABS_DISTANCE", 0x1A: "ABS_TILT_X",
             0x1B: "ABS_TILT_Y", 0x28: "ABS_MISC"}
TYPE_NAMES = {EV_SYN: "EV_SYN", EV_KEY: "EV_KEY", EV_ABS: "EV_ABS", EV_MSC: "EV_MSC"}
EVENT = struct.Struct("llHHi")  # struct input_event on 64-bit Linux

# The script (shared/usb-bridge/src/wacom_intuos5.rs). Kept independent of the
# Rust code on purpose: this side states what the host must see.
SERIAL = 0x0A2CE017
TOOL_ID = 0x802
START, CORNER, END = (16000, 12000), (48000, 12000), (48000, 28000)
HOVER_DISTANCE = 20
RAMP = [2047 * step // 16 for step in range(1, 17)]
MOVE_PRESSURE = 1024


def find_pen(vendor, product, suffix):
    """(name, event node) of the first matching input device, or None."""
    try:
        with open("/proc/bus/input/devices", encoding="utf-8") as devices:
            blocks = devices.read().split("\n\n")
    except OSError:
        return None
    for block in blocks:
        lines = block.splitlines()
        ident = next((line for line in lines if line.startswith("I:")), "")
        name = next((line for line in lines if line.startswith("N:")), "")
        handlers = next((line for line in lines if line.startswith("H:")), "")
        if f"Vendor={vendor}" not in ident or f"Product={product}" not in ident:
            continue
        name = name.partition("Name=")[2].strip().strip('"')
        if not name.endswith(suffix):
            continue
        for handler in handlers.partition("Handlers=")[2].split():
            if handler.startswith("event"):
                return name, "/dev/input/" + handler
    return None


def absinfo(fd, code):
    """(minimum, maximum, resolution) of one axis, via EVIOCGABS."""
    buffer = bytearray(24)
    try:
        fcntl.ioctl(fd, 0x80184540 + code, buffer)
    except OSError:
        return None
    _value, minimum, maximum, _fuzz, _flat, resolution = struct.unpack("6i", buffer)
    return minimum, maximum, resolution


def record(path, seconds, idle, log):
    """Every SYN_REPORT frame: (time, state after the frame, codes changed)."""
    fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
    frames, state, changed = [], {}, set()
    started = last = time.monotonic()
    try:
        for code, name in ABS_NAMES.items():
            info = absinfo(fd, code)
            if info and info[1]:
                log.write(f"    {name}: min {info[0]} max {info[1]} resolution {info[2]}\n")
        log.write("Testing ... (recording)\n")
        log.flush()
        while True:
            now = time.monotonic()
            if now - started > seconds or (frames and idle and now - last > idle):
                break
            ready, _, _ = select.select([fd], [], [], 0.2)
            if not ready:
                continue
            try:
                chunk = os.read(fd, EVENT.size * 64)
            except BlockingIOError:
                continue
            except OSError as error:
                log.write(f"device gone: {error}\n")
                break
            for offset in range(0, len(chunk) - EVENT.size + 1, EVENT.size):
                sec, usec, kind, code, value = EVENT.unpack_from(chunk, offset)
                stamp = sec + usec / 1e6
                if kind == EV_SYN and code == SYN_REPORT:
                    log.write("-------------- SYN_REPORT ------------\n")
                    frames.append((stamp, dict(state), frozenset(changed)))
                    changed = set()
                    last = time.monotonic()
                    continue
                names = {EV_KEY: KEY_NAMES, EV_ABS: ABS_NAMES}.get(kind, {})
                label = names.get(code, "?")
                if (kind, code) == (EV_MSC, MSC_SERIAL):
                    label = "MSC_SERIAL"
                log.write(f"Event: time {stamp:.6f}, type {kind} ({TYPE_NAMES.get(kind, '?')}), "
                          f"code {code} ({label}), value {value}\n")
                if kind in (EV_KEY, EV_ABS, EV_MSC):
                    state[label] = value
                    changed.add(label)
    finally:
        os.close(fd)
    return frames


def strokes(frames):
    """Frames split from BTN_TOOL_PEN 1 to BTN_TOOL_PEN 0."""
    current, found = None, []
    for frame in frames:
        _, state, changed = frame
        if "BTN_TOOL_PEN" in changed and state.get("BTN_TOOL_PEN") == 1:
            current = [frame]
        elif current is not None:
            current.append(frame)
            if "BTN_TOOL_PEN" in changed and state.get("BTN_TOOL_PEN") == 0:
                found.append(current)
                current = None
    return found


def position(state):
    return state.get("ABS_X"), state.get("ABS_Y")


def on_path(point):
    x, y = point
    horizontal = y == START[1] and START[0] <= x <= CORNER[0]
    vertical = x == END[0] and CORNER[1] <= y <= END[1]
    return horizontal or vertical


def check(stroke):
    """Every failed expectation of one stroke (empty when it passes)."""
    failures = []
    states = [state for _, state, _ in stroke]
    touching = [state for state in states if state.get("BTN_TOUCH") == 1]
    hovering = [state for state in states
                if state.get("BTN_TOUCH", 0) == 0 and state.get("BTN_TOOL_PEN") == 1]

    first = states[0]
    if first.get("MSC_SERIAL") != SERIAL:
        failures.append(f"serial {first.get('MSC_SERIAL')!r} != {SERIAL:#x}")
    if first.get("ABS_MISC") != TOOL_ID:
        failures.append(f"tool id {first.get('ABS_MISC')!r} != {TOOL_ID:#x}")
    if not any(position(s) == START and s.get("ABS_DISTANCE") == HOVER_DISTANCE
               for s in hovering):
        failures.append("no hover at the start point with distance 20")
    barrel = [s for s in states if s.get("BTN_STYLUS") == 1]
    if not barrel:
        failures.append("barrel button (BTN_STYLUS) never pressed")
    elif any(s.get("BTN_TOUCH") == 1 for s in barrel):
        failures.append("barrel button pressed while touching")
    if barrel and states[-1].get("BTN_STYLUS", 0) != 0:
        failures.append("barrel button not released")
    if not touching:
        return failures + ["the pen never touched (BTN_TOUCH)"]
    if position(touching[0]) != START:
        failures.append(f"touch-down at {position(touching[0])}, expected {START}")
    ramp = []
    for state in touching:
        if position(state) != START:
            break
        if not ramp or ramp[-1] != state.get("ABS_PRESSURE"):
            ramp.append(state.get("ABS_PRESSURE"))
    if ramp != RAMP:
        failures.append(f"pressure ramp {ramp} != {RAMP}")
    if max(s.get("ABS_PRESSURE", 0) for s in touching) != 2047:
        failures.append("pressure never reached 2047")
    path = [position(s) for s in touching]
    if CORNER not in path or path[-1] != END:
        failures.append(f"path misses the corner or ends at {path[-1]}")
    off_path = [point for point in path if not on_path(point)]
    if off_path:
        failures.append(f"{len(off_path)} touching positions off the path, e.g. {off_path[0]}")
    moving = [s.get("ABS_PRESSURE") for s in touching if position(s) not in (START, END)]
    if moving and set(moving) != {MOVE_PRESSURE}:
        failures.append(f"moving pressure {sorted(set(moving))} != {MOVE_PRESSURE}")
    lifted = states[len(states) - states[::-1].index(touching[-1]):]
    if not any(s.get("BTN_TOUCH", 0) == 0 and s.get("ABS_PRESSURE") == 0 and position(s) == END
               for s in lifted):
        failures.append("no lift-off to pressure 0 at the end point")
    if states[-1].get("BTN_TOOL_PEN") != 0:
        failures.append("the pen did not leave proximity")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--vendor", default="056a")
    parser.add_argument("--product", default="0317")
    parser.add_argument("--suffix", default=" Pen")
    parser.add_argument("--wait", type=float, default=180, help="seconds to wait for the device")
    parser.add_argument("--seconds", type=float, default=120, help="longest recording")
    parser.add_argument("--idle", type=float, default=15,
                        help="stop after this many seconds without events")
    parser.add_argument("--min-strokes", type=int, default=1)
    parser.add_argument("--log", default="pen-events.log", help="evtest-style event log")
    parser.add_argument("--json", default="pen-summary.json")
    args = parser.parse_args()

    deadline = time.monotonic() + args.wait
    found = None
    while time.monotonic() < deadline and not found:
        found = find_pen(args.vendor, args.product, args.suffix)
        if not found:
            time.sleep(0.2)
    if not found:
        print(f"FAIL: no {args.vendor}:{args.product} '*{args.suffix}' input device "
              f"within {args.wait}s")
        return 2
    name, path = found
    print(f"device: {name} at {path}", flush=True)
    with open(args.log, "w", encoding="utf-8") as log:
        log.write(f"Input device name: \"{name}\" ({path})\n")
        frames = record(path, args.seconds, args.idle, log)
    found_strokes = strokes(frames)
    results = [check(stroke) for stroke in found_strokes]
    passed = sum(1 for failures in results if not failures)
    summary = {
        "device": name,
        "node": path,
        "frames": len(frames),
        "strokes": len(found_strokes),
        "strokes_passed": passed,
        "failures": [failures for failures in results if failures],
        "pressure_max": max((s.get("ABS_PRESSURE", 0) for _, s, _ in frames), default=None),
    }
    with open(args.json, "w", encoding="utf-8") as output:
        json.dump(summary, output, indent=2)
    print(json.dumps(summary, indent=2))
    verdict = "PASS" if passed >= args.min_strokes else "FAIL"
    print(f"{verdict}: {passed}/{len(found_strokes)} strokes match the script "
          f"(need {args.min_strokes})")
    return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
