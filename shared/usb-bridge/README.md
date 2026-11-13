# arcen-usb-bridge

Platform-neutral policy, state and engines for Native tablet (USB bridged).
No OS APIs, no async runtime, no unsafe code; product adapters own device and
virtual-host-controller I/O, `arcen-protocol` owns serialization.

## What lives here (reuse before adding)

- **Admission** — `tablet_claim`, `bridged_device_guard`, `policy`,
  `descriptor`: a device is a tablet because its report descriptors declare a
  pen, never because of its vendor id; every descriptor it serves later must
  match.
- **The bridge engine** — `exporter` (`UsbExporter` over a
  `CapturedUsbDevice` backend; the Deck side), `hid_host` (`HidHostAdapter`,
  the macOS Pier's host-side HID driver), `hid_device_ipc`, `hid_publish`,
  `usbip` (the Linux Pier's USB/IP framing), `state`, `state_replay`,
  `ledger`, `backend`.
- **Desktop isolation on Linux** — `bridge_seat`: the per-port udev rule, the
  Xorg ignore file and per-desktop classes (`PressureRecalibration` off),
  node verification; `session_seat` decides which session may take a device.
- **Pen reader for known models** — `wacom_intuos_pen` (`IntuosPenReader`,
  `host_decoded_intuos_model`, `HOST_DECODED_INTUOS_MODELS`). A new model on a
  macOS Pier is an entry and its packet layout here, not a new reader.
- **Kernel package facts** — `kernel_package` (`vhci-hcd` on Rocky/RHEL).
- **Test and lab devices** — `synthetic`, `synthetic_device`,
  `descriptor_fixtures`, `wacom_intuos5` (emulated Intuos5 strokes).

Where the pen points is not here: that is `arcen-input`'s `tablet_pointer`
and `host_pointer` (ADR 0023). Consumers are registered in `consumers.json`.
