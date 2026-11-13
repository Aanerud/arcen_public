# Wacom Intuos5 touch L (`056a:0317`) descriptor fixture

"Intuos5 touch L" is the tablet's USB product string. The Linux `wacom`
driver's device table names product `0x0317` "Wacom Intuos Pro L" (type
`INTUOSPL`), so a Linux host shows its input devices as "Wacom Intuos Pro L
Pen", "… Pad" and "… Finger".

The device, configuration and HID report descriptors the emulated tablet
(`src/wacom_intuos5.rs`, `WacomIntuos5`) serves. `include_bytes!` reads the
`.bin` files; the tests in `src/wacom_intuos5_tests.rs` hold them to what the
Linux `wacom` driver expects of this model.

## Provenance

**Authored, not captured.** The tablet was not attached when this fixture
was made (2026-11-09), so no byte here was read from a device. What is taken
from the real tablet is what a Linux host logged when it enumerated it through
the bridge, and what the macOS helper logged when it admitted it:

- identity `056a:0317`, `bcdDevice` 1.00, full speed, USB HID 1.10;
- string indices: manufacturer 1 ("Wacom Co.,Ltd."), product 2
  ("Intuos5 touch L"), no serial string;
- three HID interfaces: 0 pen and pad, 1 touch, 2 boot mouse;
- interface 0 classifies as a vendor-page (`0xFF0D`) digitizer pen with no
  standard pressure or tilt usages (`evidence=VendorDigitizerPen`).

Everything else is chosen to match the Linux `wacom` driver's handling of
the Intuos5/Intuos Pro family: a 10-byte input report 2 (`WACOM_PKGLEN_INTUOS`)
and a two-byte feature report 2 (the mode switch) on interface 0, a 64-byte
input report on interface 1 (`WACOM_PKGLEN_BBTOUCH3`, the touch sensor), and
endpoints `0x81`, `0x82`, `0x83`. The report descriptors are the same shapes as
`descriptor_fixtures::INTUOS5_STYLE_*`.

| File | Bytes | Hex |
| --- | --- | --- |
| `device.bin` | 18 | `12010002000000406a051703000101020001` |
| `configuration.bin` | 84 | `09025400030100803209040000010300000009211001000122370007058103100001090401000103000000092110010001221f000705820340000109040200010301020009211001000122320007058303090004` |
| `report-interface0.bin` | 55 | `060dff0901a10185020900150026ff007508950981028503090095098102850209009501b102850309009501b10285040900950fb102c0` |
| `report-interface1.bin` | 31 | `0600ff0901a10185110901150026ff007508953f810285120901953fb102c0` |
| `report-interface2.bin` | 50 | `05010902a1010901a100050919012903150025019503750181029501750581030501093009311581257f750895028106c0c0` |

## Replacing it with the real descriptors

With the tablet attached to a Mac, run
`scripts/lab/wacom-replay/capture-descriptors.py` (read-only: the registry and
libusb's cached configuration, nothing is opened or sent). It compares the
real bytes with these. `--write` saves them to
`../wacom-intuos5-touch-l-captured/` and never overwrites this authored
fixture: bytes read from a device are Wacom's descriptors, not Arcen's work.
That directory is deliberately not annotated in `REUSE.toml` (this fixture's
five files are listed by exact path), so `reuse lint` fails until
Release/Security decides their licensing. Only then may the captured bytes
replace these files, after which `cargo test -p arcen-usb-bridge wacom`
names anything the driver would see differently.
