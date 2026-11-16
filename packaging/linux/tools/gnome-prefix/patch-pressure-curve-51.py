#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
# Lab only, applied to Mutter 51.1 (unchanged on main as of 2026-11-15): the tablet
# pressure clients see is the pressure curve's value, not the pressure times it
# (GNOME/mutter#5077, a regression from e7ba16689 in 46), and the curve's 256-sample
# lookup interpolates by the position's fraction between two samples, with the last
# sample at x = 1.0. With GNOME's default linear curve a pen's pressure then reaches
# clients unchanged instead of squared.
# Usage: patch-pressure-curve-51.py <mutter source dir>
import sys

root = sys.argv[1]


def patch(relative, old, new):
    path = f"{root}/{relative}"
    with open(path) as file:
        source = file.read()
    if new in source:
        print(f"already patched: {relative}")
        return
    assert source.count(old) == 1, f"context not found in {relative}"
    with open(path, "w") as file:
        file.write(source.replace(old, new))
    print(f"patched {relative}")


patch(
    "src/backends/native/meta-input-device-tool-native.c",
    """  MetaInputDeviceToolNative *evdev_tool;
  double factor;

  g_return_val_if_fail (META_IS_INPUT_DEVICE_TOOL_NATIVE (tool), pressure);

  evdev_tool = META_INPUT_DEVICE_TOOL_NATIVE (tool);

  pressure = CLAMP (pressure, 0.0, 1.0);
  factor = meta_bezier_lookup (evdev_tool->bezier, pressure);

  return pressure * factor;
}""",
    """  MetaInputDeviceToolNative *evdev_tool;

  g_return_val_if_fail (META_IS_INPUT_DEVICE_TOOL_NATIVE (tool), pressure);

  evdev_tool = META_INPUT_DEVICE_TOOL_NATIVE (tool);

  pressure = CLAMP (pressure, 0.0, 1.0);

  return meta_bezier_lookup (evdev_tool->bezier, pressure);
}""",
)

patch(
    "src/backends/native/meta-bezier.c",
    """  /* The position may be fine-grained than our calculated bezier curve,
   * find the two closest points and do a linear interpolation between
   * those two points.
   */
  int low_idx = CLAMP ((int)(pos * b->precision), 0, b->precision - 1);
  int high_idx = CLAMP (low_idx + 1, 0, b->precision - 1);
  double low = b->points[low_idx];
  double high = b->points[high_idx];
  double rval = low + (high - low) * (pos - (int)pos);

  return rval;
}""",
    """  /* The position may be fine-grained than our calculated bezier curve,
   * find the two closest points and do a linear interpolation between
   * those two points, by the position's fraction of the distance between
   * them.
   *
   * Sample i holds the curve at x = i / precision, except the last
   * sample, which holds the curve's end at x = 1.0 (see
   * meta_bezier_sample()), so the last interval is twice as wide.
   */
  int last_idx = (int) b->precision - 1;
  double x = CLAMP (pos, 0.0, 1.0) * b->precision;
  int low_idx;
  int high_idx;
  double low_x;
  double high_x;
  double low;
  double high;

  if (last_idx < 1)
    return b->points[0];

  low_idx = CLAMP ((int) x, 0, last_idx - 1);
  high_idx = low_idx + 1;
  low_x = low_idx;
  high_x = high_idx == last_idx ? (double) b->precision : high_idx;
  low = b->points[low_idx];
  high = b->points[high_idx];

  return low + (high - low) * (x - low_x) / (high_x - low_x);
}""",
)
