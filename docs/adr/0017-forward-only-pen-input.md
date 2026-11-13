# ADR 0017: Forward-only pen input

**Status:** Accepted (2026-11-09), for 0.19.0. Settles the
Windows half of [ADR 0012](0012-hard-usb-on-windows-hosts.md), which stays
as the record of the route that is not taken.

> Arcen gives a remote desktop a pen through each operating system's own
> pointer and pen APIs. It does not reimplement a 1990s tablet API, and it
> does not offer a mode that would need a signed Windows kernel driver.

## Context

**Wintab is not part of Windows.** It is an early-1990s tablet API.
Applications load `wintab32.dll`, and that DLL comes from each tablet
maker's driver, not from Windows. A Windows host with no tablet attached has
no Wintab, so an application set to Wintab sees no tablet over any remote
desktop that does not ship one.

**The applications Arcen's users draw in already have a Windows Ink mode.**
Research on 2026-11-08 checked the current documentation of Photoshop,
Krita, Clip Studio Paint, Blender, ZBrush, Substance 3D Painter, Maya, Toon
Boom Harmony, TVPaint, Corel Painter, Rebelle, Houdini 21 and Mari 4.8 and
later. Each one offers Windows Ink, pointer input or Real-Time Stylus. The
one confirmed Wintab-only title is Mari 4.7 and earlier.

**A Windows Pier already gives the session a real Windows Ink pen**, from
user mode, with no driver: `PenInjector` in `hosts/windows/src/input.rs`
creates a synthetic `PT_PEN` device (`CreateSyntheticPointerDevice`) and
sends each sample through `InjectSyntheticPointerInput`, with pressure
(`0..=1024`, the Windows Ink ceiling), tilt, rotation, eraser and barrel
button.

**The routes to Wintab all cost more than they return:**

- **Arcen's own `wintab32.dll`.** It would replace the tablet maker's DLL in
  `System32` and `SysWOW64`, and the maker's next driver update would
  replace it back. Every application that loads Wintab would load Arcen code
  into its process. It needs a feeder from the Pier into every such process.
- **A virtual Wacom tablet** for the maker's own driver to claim. Products
  that do this present a virtual USB device with Wacom's USB vendor id and
  product name, and need the Wacom driver installed on the host. That brings
  in a signed kernel driver, a vendor id that the USB-IF assigns for its
  owner's exclusive use, a trademark, and a driver licence written for real
  Wacom hardware. Arcen treats every pen tablet the same, whoever made it.
- **Native tablet (USB bridged) on Windows**, which would let the host's own
  tablet driver, and its Wintab, claim the real device. Windows has no
  virtual USB host controller in the box. ADR 0012 records the route: a
  KMDF driver on `UdeCx`. Shipping a kernel driver needs an EV code-signing
  certificate, Microsoft Hardware Dev Center (Partner Center) attestation
  signing, and WHQL/HLK for broad distribution, including Windows Server.
  The project does not fund that pipeline.

**macOS has a supported route, and Arcen holds its entitlements.** The Deck captures
a tablet for Native tablet with `com.apple.developer.accessory-access.usb`
(`packaging/macos/Deck-usb-hard.entitlements`). A macOS Pier can present a
virtual HID device with `com.apple.developer.hid.virtual.device`
(`packaging/macos/Pier-virtual-hid.entitlements`). Both are Apple
entitlements granted to the project, not drivers it must sign and service.

## Decision

1. **Arcen does not implement or support Wintab.** A Windows Pier delivers
   pens through Windows Ink and pointer input only: `PenInjector`,
   `CreateSyntheticPointerDevice(PT_PEN, …)` and
   `InjectSyntheticPointerInput` in `hosts/windows/src/input.rs`. Arcen
   ships no `wintab32.dll`, injects no DLL into applications, and emulates
   no tablet maker's device, vendor id or product name.
2. **Native tablet (USB bridged) is not offered on Windows hosts.** A
   Windows Pier advertises `usb_tablet_bridge` as `Unavailable`
   (`hosts/windows/src/session.rs`) and refuses a request with a reason
   (`resolve_windows_tablet_mode_result`). **Tablet support** (the typed
   pen) is the Windows pen.
3. **Native tablet stays where the platform offers a supported route:**
   Linux lab builds (`usb-hard-lab`, `usb-vhci`) and macOS, through the two
   Apple entitlements above.
4. **The forward path is one typed-device model.** The Deck announces each
   input device it holds, by type and capability, and the host builds the
   matching OS-native virtual device: a pen through Windows Ink, `uinput`
   or a macOS virtual HID device. Moonlight's controller announcement is the
   model learnt from; it works with any tablet, at any distance.
   [ADR 0018](0018-unified-typed-devices.md) records that design.

## Consequences

- **Wintab-only applications get no pressure on a Windows Pier.** Mari 4.7
  and earlier is the known case. Users set each drawing application to
  Windows Ink:
  [Windows applications: choose Windows Ink](../operations/pen-tablet-input.md#4a-windows-applications-choose-windows-ink).
- **Pressure on Windows has 1,025 steps** whatever the tablet offers. That is
  the Windows Ink ceiling, and Arcen does not work around it.
- **Finger touch and a tablet's own buttons are not sent to a Windows
  host.** They keep working on the Mac, as with Tablet support everywhere.
- **No kernel driver, DLL or vendor identity is added to the Windows
  package** for pens, so nothing new needs signing, servicing or a licence
  review.
- **The clean-room Wintab shim on the roadmap is withdrawn.**
- **ADR 0012 is not built.** It stays as the record of the route. Revisit
  this decision only if a signing pipeline is funded; the microphone driver,
  which needs the same pipeline, is a separate decision.
