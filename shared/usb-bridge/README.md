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
- **Pen readers (any tablet by default)** — `generic_pen` (`GenericPenLayout`,
  `GenericPenReader`: any pen by its own report descriptor, bounded for
  untrusted input), `wacom_intuos_pen` (`IntuosPenReader`,
  `HOST_DECODED_INTUOS_MODELS`: a dedicated reader where one adds features),
  `host_pen` (`host_pen_route`: which one a host uses). A tablet a standard
  descriptor describes needs nothing here; a vendor packet format is a
  dedicated reader. `generic_pen_tablet` is the lab emulation (pid.codes
  test identity).
- **The Deck's USB component** — `native_component`
  (`NativeUsbComponentState`, `NativeComponentFacts::resolve`): where the
  optional privileged component stands (the macOS helper, the Windows
  service) and the one wording both Decks show for it, with the Native
  tablet option unavailable until `Ready`; `component_ipc`: the framing and
  handshake of the Deck↔component boundary (both sides, every platform);
  `component_session`: one Deck connection served by a component (hello,
  info, one bounded writer, first-request deadline, abort on write failure)
  over any stream, with the platform's `arcen-protocol` URB codec;
  `winusb_package`: the Windows component's per-tablet `WinUSB` INF and
  catalog definition, the claim journal whose release and recovery plans
  put a PC back as it was (proven on Secure Boot, matrix §20), the service
  and pipe names, and `TabletPreparation` (which attached tablet is
  prepared, or which one "Prepare this tablet…" prepares). The Deck's half
  of the link, a generic client over any local stream, is
  `arcen-deck-core`'s `runtime::usb_component` (it needs tokio, which this
  crate does not link). `component_lease`: how long a Deck may hold the
  tablet — granted for the console session and user verified at accept,
  revoked on a console disconnect or switch, a remote takeover, a logoff,
  the authorising Deck's exit and a component stop; **a screen lock by the
  same user keeps it**. `component_session::SessionRevocation` ends a live
  session for it: no further request served, the Deck told why, the stream
  aborted, in-flight transfers cancelled.
- **Kernel package facts** — `kernel_package` (`vhci-hcd` on Rocky/RHEL).
- **Test and lab devices** — `synthetic`, `synthetic_device`,
  `descriptor_fixtures`, `wacom_intuos5` (emulated Intuos5 strokes).

Where the pen points is not here: that is `arcen-input`'s `tablet_pointer`
and `host_pointer` (ADR 0023). Consumers are registered in `consumers.json`.
