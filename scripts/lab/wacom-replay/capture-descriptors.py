#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS
"""Read an attached Wacom Intuos5's descriptors on macOS without capturing it.

Read-only: nothing is opened, claimed or sent to the device.

- The device descriptor is rebuilt from the IOUSBHostDevice registry
  properties (bcdUSB, bDeviceClass, ..., bNumConfigurations).
- Each HID interface's report descriptor is the `ReportDescriptor` property
  the HID stack published (`IOHIDDevice`), as the privileged helper reads it.
- The configuration descriptor comes from libusb's cached copy
  (`libusb_get_config_descriptor`, which on macOS reads the descriptor the
  USB stack already holds and does not open the device), rebuilt byte for
  byte from its fields and `extra` blocks. Needs a libusb dylib (Homebrew's
  is enough); without one, the configuration is reported as unavailable.

Compares everything with the Arcen-authored fixture in
shared/usb-bridge/fixtures/wacom-intuos5-touch-l/. With --write it saves the
captured bytes to shared/usb-bridge/fixtures/wacom-intuos5-touch-l-captured/
and never touches the authored fixture: captured descriptors are Wacom's
bytes, not Arcen's work, so that directory carries no REUSE annotation and
`reuse lint` fails until Release/Security decides how to license them.

The tablet is "Intuos5 touch L" by its USB product string; the Linux `wacom`
driver names product 0x0317 "Intuos Pro L".
"""

import argparse
import ctypes
import ctypes.util
import os
import plistlib
import subprocess
import sys

VENDOR, PRODUCT = 0x056A, 0x0317
FIXTURES = os.path.join(os.path.dirname(__file__), "..", "..", "..", "shared", "usb-bridge",
                        "fixtures")
FIXTURE = os.path.join(FIXTURES, "wacom-intuos5-touch-l")
CAPTURED = os.path.join(FIXTURES, "wacom-intuos5-touch-l-captured")
DEVICE_FIELDS = [("bcdUSB", 2), ("bDeviceClass", 1), ("bDeviceSubClass", 1),
                 ("bDeviceProtocol", 1), ("bMaxPacketSize0", 1), ("idVendor", 2),
                 ("idProduct", 2), ("bcdDevice", 2), ("iManufacturer", 1), ("iProduct", 1),
                 ("iSerialNumber", 1), ("bNumConfigurations", 1)]


def registry(class_name):
    output = subprocess.run(["ioreg", "-a", "-l", "-w0", "-r", "-c", class_name],
                            check=True, capture_output=True).stdout
    return plistlib.loads(output) if output.strip() else []


def walk(entry, parents=()):
    yield entry, parents
    for child in entry.get("IORegistryEntryChildren", []):
        yield from walk(child, parents + (entry,))


def device_descriptor():
    for root in registry("IOUSBHostDevice"):
        for entry, _ in walk(root):
            if entry.get("idVendor") == VENDOR and entry.get("idProduct") == PRODUCT:
                data = bytearray([18, 1])
                for name, size in DEVICE_FIELDS:
                    data += int(entry[name]).to_bytes(size, "little")
                return bytes(data), entry
    return None, None


def report_descriptors(device_entry):
    found = {}
    for entry, parents in walk(device_entry):
        descriptor = entry.get("ReportDescriptor")
        if not isinstance(descriptor, bytes):
            continue
        interface = next((parent["bInterfaceNumber"] for parent in reversed(parents)
                          if "bInterfaceNumber" in parent), entry.get("bInterfaceNumber"))
        if interface is not None:
            found.setdefault(int(interface), descriptor)
    return found


class Endpoint(ctypes.Structure):
    _fields_ = [("bLength", ctypes.c_uint8), ("bDescriptorType", ctypes.c_uint8),
                ("bEndpointAddress", ctypes.c_uint8), ("bmAttributes", ctypes.c_uint8),
                ("wMaxPacketSize", ctypes.c_uint16), ("bInterval", ctypes.c_uint8),
                ("bRefresh", ctypes.c_uint8), ("bSynchAddress", ctypes.c_uint8),
                ("extra", ctypes.POINTER(ctypes.c_uint8)), ("extra_length", ctypes.c_int)]


class AltSetting(ctypes.Structure):
    _fields_ = [("bLength", ctypes.c_uint8), ("bDescriptorType", ctypes.c_uint8),
                ("bInterfaceNumber", ctypes.c_uint8), ("bAlternateSetting", ctypes.c_uint8),
                ("bNumEndpoints", ctypes.c_uint8), ("bInterfaceClass", ctypes.c_uint8),
                ("bInterfaceSubClass", ctypes.c_uint8), ("bInterfaceProtocol", ctypes.c_uint8),
                ("iInterface", ctypes.c_uint8), ("endpoint", ctypes.POINTER(Endpoint)),
                ("extra", ctypes.POINTER(ctypes.c_uint8)), ("extra_length", ctypes.c_int)]


class Interface(ctypes.Structure):
    _fields_ = [("altsetting", ctypes.POINTER(AltSetting)), ("num_altsetting", ctypes.c_int)]


class Config(ctypes.Structure):
    _fields_ = [("bLength", ctypes.c_uint8), ("bDescriptorType", ctypes.c_uint8),
                ("wTotalLength", ctypes.c_uint16), ("bNumInterfaces", ctypes.c_uint8),
                ("bConfigurationValue", ctypes.c_uint8), ("iConfiguration", ctypes.c_uint8),
                ("bmAttributes", ctypes.c_uint8), ("MaxPower", ctypes.c_uint8),
                ("interface", ctypes.POINTER(Interface)), ("extra", ctypes.POINTER(ctypes.c_uint8)),
                ("extra_length", ctypes.c_int)]


class DeviceDescriptor(ctypes.Structure):
    _fields_ = [("bLength", ctypes.c_uint8), ("bDescriptorType", ctypes.c_uint8),
                ("bcdUSB", ctypes.c_uint16), ("bDeviceClass", ctypes.c_uint8),
                ("bDeviceSubClass", ctypes.c_uint8), ("bDeviceProtocol", ctypes.c_uint8),
                ("bMaxPacketSize0", ctypes.c_uint8), ("idVendor", ctypes.c_uint16),
                ("idProduct", ctypes.c_uint16), ("bcdDevice", ctypes.c_uint16),
                ("iManufacturer", ctypes.c_uint8), ("iProduct", ctypes.c_uint8),
                ("iSerialNumber", ctypes.c_uint8), ("bNumConfigurations", ctypes.c_uint8)]


def extra(pointer, length):
    return bytes(pointer[:length]) if length > 0 and pointer else b""


def configuration_descriptor():
    """The cached configuration descriptor via libusb, without opening the device."""
    path = ctypes.util.find_library("usb-1.0") or "/opt/homebrew/lib/libusb-1.0.dylib"
    try:
        libusb = ctypes.CDLL(path)
    except OSError:
        return None, "libusb not found"
    context = ctypes.c_void_p()
    if libusb.libusb_init(ctypes.byref(context)) != 0:
        return None, "libusb_init failed"
    devices = ctypes.POINTER(ctypes.c_void_p)()
    count = libusb.libusb_get_device_list(context, ctypes.byref(devices))
    try:
        for index in range(max(count, 0)):
            device = ctypes.c_void_p(devices[index])
            descriptor = DeviceDescriptor()
            if libusb.libusb_get_device_descriptor(device, ctypes.byref(descriptor)) != 0:
                continue
            if (descriptor.idVendor, descriptor.idProduct) != (VENDOR, PRODUCT):
                continue
            config = ctypes.POINTER(Config)()
            if libusb.libusb_get_config_descriptor(device, 0, ctypes.byref(config)) != 0:
                return None, "libusb_get_config_descriptor failed"
            try:
                c = config.contents
                body = extra(c.extra, c.extra_length)
                for number in range(c.bNumInterfaces):
                    interface = c.interface[number]
                    for alternate in range(interface.num_altsetting):
                        a = interface.altsetting[alternate]
                        body += bytes([a.bLength, a.bDescriptorType, a.bInterfaceNumber,
                                       a.bAlternateSetting, a.bNumEndpoints, a.bInterfaceClass,
                                       a.bInterfaceSubClass, a.bInterfaceProtocol, a.iInterface])
                        body += extra(a.extra, a.extra_length)
                        for e in range(a.bNumEndpoints):
                            p = a.endpoint[e]
                            body += bytes([p.bLength, p.bDescriptorType, p.bEndpointAddress,
                                           p.bmAttributes]) + p.wMaxPacketSize.to_bytes(2, "little")
                            body += bytes([p.bInterval]) + extra(p.extra, p.extra_length)
                header = bytes([c.bLength, c.bDescriptorType]) + c.wTotalLength.to_bytes(2, "little")
                header += bytes([c.bNumInterfaces, c.bConfigurationValue, c.iConfiguration,
                                 c.bmAttributes, c.MaxPower])
                data = header + body
                if len(data) != c.wTotalLength:
                    return data, f"rebuilt {len(data)} bytes, wTotalLength says {c.wTotalLength}"
                return data, None
            finally:
                libusb.libusb_free_config_descriptor(config)
        return None, "libusb does not see the tablet"
    finally:
        if count > 0:
            libusb.libusb_free_device_list(devices, 1)
        libusb.libusb_exit(context)


def compare(name, captured, write):
    path = os.path.join(FIXTURE, name)
    with open(path, "rb") as fixture:
        current = fixture.read()
    verdict = "same" if captured == current else "DIFFERENT"
    print(f"{name}: captured {len(captured)} bytes, fixture {len(current)} bytes: {verdict}")
    print(f"  {captured.hex()}")
    if write:
        os.makedirs(CAPTURED, exist_ok=True)
        target = os.path.join(CAPTURED, name)
        with open(target, "wb") as output:
            output.write(captured)
        print(f"  wrote {os.path.normpath(target)} (unlicensed until Release/Security annotates it)")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--write", action="store_true",
                        help="save the captured bytes to the separate, unannotated "
                             "wacom-intuos5-touch-l-captured/ directory")
    args = parser.parse_args()
    device, entry = device_descriptor()
    if device is None:
        print(f"no {VENDOR:04x}:{PRODUCT:04x} in the I/O Registry; attach the tablet")
        return 2
    print(f"tablet: {entry.get('USB Vendor Name')} {entry.get('USB Product Name')} "
          f"speed={entry.get('Device Speed')} location={entry.get('locationID', 0):#x}")
    compare("device.bin", device, args.write)
    reports = report_descriptors(entry)
    for interface in sorted(reports):
        compare(f"report-interface{interface}.bin", reports[interface], args.write)
    if not reports:
        print("no ReportDescriptor in the registry (is a HID driver bound?)")
    configuration, problem = configuration_descriptor()
    if configuration is not None and problem is None:
        compare("configuration.bin", configuration, args.write)
    else:
        print(f"configuration: unavailable ({problem})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
