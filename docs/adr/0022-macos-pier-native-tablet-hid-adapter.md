# ADR 0022: Native tablet on a macOS Pier through a USB-to-HID adapter and `IOHIDUserDevice`

**Status:** Accepted; shipped in 0.19.0 (first proposed 2026-11-10 for lab
builds). A Mac Deck's bridged tablet reaches a macOS Pier as HID devices
published with `IOHIDUserDevice`, using only the processes and the
entitlement the Pier already has; nothing new is requested from Apple. Since
2026-11-12 (§14) the Pier reads a known model's pen itself.

**0.19.0 status note (2026-11-12):** the adapter ships in the release macOS
Pier (Cargo feature `native-tablet`, default) and is offered when the Pier is
signed with the virtual-HID entitlement (`build-pier-pkg.sh
--with-virtual-hid`, as release packages are) and its probe passes; the
entitlement is the gate, the Pier does not read `usb.native_tablet`.
Pressure: since 0.19.0 (§14.1) the Pier reads the pen itself for a model
whose packets Arcen knows (the Intuos5 touch L) and posts CoreGraphics
tablet events with pressure, which reverses this record's "never interpret"
for the macOS Pier; any other model is refused (§14.2), because macOS
publishes bridged devices as virtual and the tablet maker's driver does not
bind them (§10). A DriverKit HID driver was researched and set aside (§14).
`probe-bridged-hid` writes the Wacom mode switch and is built only with the
lab feature `lab-bridged-hid-probe`. Measured on the lab macOS Pier with
the emulated Intuos5 (`NSEvent` pressure 0.059 to 1.000, 17 levels in the
scripted stroke), then proven with a real Intuos5 bridged from a Mac Deck.

> Scope: the public API in `arcen-usb-bridge` (`hid_host`,
> `hid_device_ipc`, the Intuos pen reader), superseding
> [ADR 0019](0019-iousbhost-usb-backend.md) §6, devices published with a real
> vendor's identity on the host, feature and output reports an authenticated
> Deck can drive, and a mode of the entitled `hid-injector` child. The
> decision list is in §12.

## Context

Native tablet (the USB bridge, `UsbTabletBridge`) forwards a Deck's physical
pen tablet to the Pier as USB traffic. The Deck's root USB helper
(`arcen-usb-helper`, IOUSBHost backend, [ADR 0019](0019-iousbhost-usb-backend.md))
captures the device; the shared exporter (`arcen_usb_bridge::UsbExporter`)
answers the Pier's URBs from it. The Pier is the USB *host*: it submits
`UsbBridgeUrbSubmit`/`UsbBridgeUrbCancel` frames and receives
`UsbBridgeUrbComplete`. On Linux the host is the kernel: a virtual host
controller (`vhci-hcd`, [ADR 0020](0020-linux-native-tablet-in-tree-vhci-hcd.md))
enumerates the device and the in-kernel `wacom` driver binds it
([ADR 0021](0021-bridged-usb-devices-on-their-own-seat.md)).

macOS has a virtual host controller API too
(`IOUSBHostControllerInterface`), and ADR 0019 §6 planned to use it. It needs
`com.apple.developer.usb.host-controller-interface`, which the
`pier.arcen.tech` profile does not carry (`hosts/macos/SIGNING.md`). Arcen
does not wait for it.

What the Pier does hold, measured and shipped since 0.18.0:

1. `com.apple.developer.hid.virtual.device` on the `pier.arcen.tech`
   signature, which is what `IOHIDUserDeviceCreateWithProperties` checks
   (`IOHIDUserDevice.h`, lines 118-121). Without it creation returns `NULL`;
   root is no substitute (`hosts/macos/AGENTS.md`, "Virtual HID, and what it
   gates").
2. Creation also needs Accessibility for the app *responsible* for the
   creating process. The Agent Helper (`pier.arcen.tech.agent`) holds that
   grant but **no entitlement** — it is signed without an entitlements file
   (`hosts/macos/SIGNING.md`, "Entitlements: one in the shipping build"). So
   the helper already starts the Pier's own executable as its child,
   `arcen-pier-macos hid-injector`, which owns virtual HID devices and nothing
   else (`hosts/macos/src/input/virtual_keyboard.rs`,
   `docs/security/macos-pier-process-model.md`).
3. A *generic* HID digitizer published this way reaches applications as
   proximity without pressure, tilt or rotation (`hosts/macos/AGENTS.md`, "A
   virtual pen digitizer, measured"). Pressure needs the tablet maker's driver
   on the host, which matches a device by vendor, product and transport and
   speaks the vendor protocol (for a Wacom Intuos: switch to tablet mode with
   `SET_REPORT(Feature 2)`, then parse vendor-page reports).
4. `IOHIDUserDevice` supports exactly this (Apple's
   [IOHIDUserDevice](https://developer.apple.com/documentation/iokit/iohiduserdevice)
   documentation): a device created with the real vendor/product id, the
   device's own report descriptor and `Transport`, whose `GET_REPORT`/`SET_REPORT`
   callbacks can be forwarded to the client as USB control requests.

`hosts/macos/SIGNING.md` objected that a HID-only sink "would have to parse USB
transfers to recover reports, which is interpretation, and it has nowhere to
carry the control transfers a Wacom tablet needs". Both halves have an answer
that keeps the mode's promise — Arcen never interprets the *device's* data
(superseded for the macOS Pier by §14.1: a known
model's pen is read by the host):

* An interrupt IN transfer of a HID interface *is* one input report, byte for
  byte (HID 1.11 §8.4). The adapter forwards it verbatim; it parses only the
  standard USB framing (descriptors), which `BridgedDeviceGuard` already
  parses on Linux.
* `IOHIDUserDevice` has `IOHIDUserDeviceRegisterSetReportBlock` and
  `IOHIDUserDeviceRegisterGetReportBlock`. Every feature and output report the
  host driver sets or reads becomes a HID class `SET_REPORT`/`GET_REPORT`
  control transfer over the unchanged bridge.

What it cannot carry is a non-HID interface or a device that needs USB-level
requests beyond HID class ones. Those are already refused by the shared policy
(every bridged interface must be HID, `admissible_tablet`).

## Decision

### 1. Data flow

```
Deck                                    Pier (macOS)
────                                    ────────────
physical tablet
  │ IOUSBHost (root arcen-usb-helper,    network service (_arcen, UDP 18444)
  │ unchanged)                             │  QUIC bulk lane ⇄ agent socket
UsbExporter (unchanged)                    │  (PierLaneRelay, unchanged)
  │  UsbBridgeUrbComplete  ──────────────► │
  │  ◄────────────── UsbBridgeUrbSubmit/Cancel
                                           ▼
                                        session (Agent Helper; `serve` in dev)
                                          bridged_tablet.rs
                                            HidHostAdapter (shared, §3)
                                              ├─ BridgedDeviceGuard (shared)
                                              └─ enumeration / polling / reports
                                           │  stdin/stdout frames (hid_device_ipc)
                                           ▼
                                        hid-injector --bridged-usb
                                          (existing child, pier.arcen.tech,
                                           entitled; Accessibility of its
                                           responsible app, the Agent Helper)
                                            one IOHIDUserDevice per HID interface
                                           │
                                           ▼
                        Wacom's driver (if installed) or macOS generic HID
                                           │
                                           ▼
                                      applications
```

The wire is the unchanged Hard USB bridge v1: no new message, frame type or
field. The network service is unchanged: USB frames are already bulk records
that `PierLaneRelay` carries between the Deck and the agent's session socket
(`shared/transport/src/quic/lane_relay.rs`, `lane_for_binary_message`).

### 2. Where it runs: no new helper, daemon, dext or process kind

* **The USB host logic runs in the session**, which is the Agent Helper in the
  installed product (and the single `serve` process in development). It has
  the Deck's frames, the session's lifetime, and the policy.
* **The devices live in the existing `hid-injector` child**, started in a
  `--bridged-usb` mode. It is the same executable, the same `pier.arcen.tech`
  signature and the same parent as the keyboard/pointer injector; it reads
  frames on standard input and dies with its input exactly as that one does.
  It holds no network, session or policy.
* **Why not the Agent Helper itself:** its signature carries no entitlement,
  and creation there returns `NULL` (measured, item 2 of the context). Signing
  the helper with the entitlement would need a profile for
  `pier.arcen.tech.agent`, i.e. a new request to Apple, which is out of
  scope.
* The child is started per Native-tablet session, the way the keyboard child
  is started per session for the `virtual_hid` input backends. A session that
  also uses a virtual keyboard has two children of the same kind; merging them
  into one is a follow-up, not a requirement of this design.

### 3. The adapter acts as the USB host driver (shared, `arcen_usb_bridge::hid_host`)

`HidHostAdapter` is a sans-I/O state machine in `shared/usb-bridge`
(`#![forbid(unsafe_code)]`, no new dependencies). It produces URB requests and
consumes completions; a platform turns them into wire frames and devices.
Every descriptor completion passes `BridgedDeviceGuard` before it is used.

Enumeration, in order (each a control transfer on endpoint 0, `timeout_ms`
1000, at most one control transfer in flight):

| Step | Request | On failure |
| --- | --- | --- |
| 1 | `GET_DESCRIPTOR(Device, 18)` | fatal; guard checks identity, class, one configuration |
| 2 | `GET_DESCRIPTOR(Configuration, 9)` → `wTotalLength` (≤ 4 KiB) | fatal |
| 3 | `GET_DESCRIPTOR(Configuration, wTotalLength)` | fatal; guard: HID-only, interrupt-only, exactly the authorized interfaces, advertised report lengths |
| 4 | `GET_DESCRIPTOR(String)` for manufacturer and product (language 0x0409, ≤ 255 bytes) | tolerated; property omitted |
| 5 | `SET_CONFIGURATION(bConfigurationValue)` | fatal (the Deck acknowledges the active configuration locally) |
| 6 | per interface: `SET_IDLE(0)` | a stall is tolerated (HID 1.11 §7.2.4 makes it optional) |
| 7 | per interface: `GET_DESCRIPTOR(Report, length)` | fatal; guard: exactly the authorized bytes |

Then the adapter publishes the plan (§4) and, once the devices exist, keeps
**one interrupt IN URB outstanding per HID interface's IN endpoint**. Each
successful completion is one report, delivered verbatim to that interface's
device with `IOHIDUserDeviceHandleReportWithTimeStamp` (host-side
`mach_absolute_time()` at delivery; the bridge carries no device timestamp)
and the URB is resubmitted. The Deck holds an interrupt IN until the device
reports (`hold_until_report`), so an idle tablet costs nothing.

Report requests from the host driver:

| Callback | Becomes | Answer |
| --- | --- | --- |
| set report, type Output or Feature | `SET_REPORT` (0x21, 0x09, `type << 8 \| id`, interface) with the report bytes | `kIOReturnSuccess` on success, an error on stall/timeout |
| get report, type Input or Feature | `GET_REPORT` (0xA1, 0x01, …, length = buffer) | the bytes, or an error |

Output reports go through the control pipe even where an interface has an
interrupt OUT endpoint (allowed by HID 1.11 §7.2.2); the guard admits only
interrupt endpoints, and v1 tablets use the control pipe for their mode and
LED reports.

`SET_PROTOCOL` is never sent: devices start in report protocol, which is what
a report descriptor describes.

**Bounds** (all enforced in shared code and tested):

| Bound | Value |
| --- | --- |
| HID interfaces / devices per attachment | ≤ `MAX_INTERFACES` (16) |
| URBs in flight | one control + one interrupt IN per interface |
| Queued report requests | 8; more are refused with an error at once |
| One report / control payload | ≤ `MAX_TRANSFER_BYTES` (16 KiB) |
| Interrupt IN length | the interface's largest declared input report (≥ `wMaxPacketSize`), ≤ 16 KiB |
| Enumeration | must finish within 10 s |
| A control transfer the Deck never answers | failed after 3 s (the wire timeout is 1 s); a report request then answers with an error, enumeration fails the attachment |
| Consecutive interrupt errors on one endpoint | 8, then the attachment fails |
| An interrupt IN outstanding for 10 s (`INTERRUPT_IDLE_RESUBMIT`) | cancelled and submitted afresh. The Deck answers every cancel with exactly one Cancelled, even for a transfer that already completed, and its helper may send that transfer's own completion before or after the Cancelled (two threads). So the old transfer stays buried until its Cancelled, then retired: a Success that raced the cancel, in either order, is delivered once as a report (it precedes anything the fresh transfer brings), other statuses are ignored. An idle tablet costs one cancel and one submit per interface every 10 s; a lost submit or completion silences an interface for at most that long |
| Buried (cancelled) transfers awaiting their Cancelled, and retired ones | ≤ 32 each, oldest dropped first, so lost cancels cannot pin memory; a timed-out report request's late answer is dropped, before or after its Cancelled (the request already failed) |
| Device descriptor / configuration header | exactly 18 / 9 bytes with the right type, or the attachment fails (`HidHostError::Descriptor`); the published identity is the authorized one, never a served one |
| String properties | ≤ 126 characters, control characters removed |
| A completion for an unknown URB | fatal (a confused or hostile peer) |

### 4. The device plan and its properties

One `IOHIDUserDevice` per HID interface, each with:

| Key | Value |
| --- | --- |
| `ReportDescriptor` | the interface's report descriptor, exactly as authorized and as served |
| `VendorID`, `ProductID`, `VersionNumber` | `idVendor`, `idProduct`, `bcdDevice` of the served device descriptor (equal to the authorized identity) |
| `Manufacturer`, `Product` | the device's string descriptors, sanitized; `Product` falls back to "Arcen bridged tablet" |
| `SerialNumber` | a session-scoped value `arcen-<generation hex>-<interface>`, never the device's own serial (§12) |
| `Transport` | `"USB"` — requested; macOS 26.7 publishes every device of an unprivileged creator as `"Virtual"` whatever it is given (measured, §13) |
| `LocationID` | `0xA2C0_0000 \| (generation & 0xFFF) << 8 \| interface` — stable within an attachment, distinct across interfaces and attachments |
| `PrimaryUsagePage`, `PrimaryUsage` | the first top-level collection of the report descriptor |
| `CountryCode` | `bCountryCode` of the interface's HID class descriptor |
| `RequestTimeout` | 3 000 000 µs, matching the adapter's control deadline |

The real vendor and product id are what a vendor driver matches on
(`IOKitPersonalities` match on `idVendor`/`idProduct`); the location makes the device
look like what it is, one interface of one USB device. The transport cannot:
the HID system replaces it with `"Virtual"` and marks the device
`HIDVirtualDevice`, `Privileged = No` (Apple's own built-in virtual devices
keep `"USB"` and `Privileged = Yes`). Anything that filters on `Transport` or
looks for a USB parent in the registry does not see a USB device.

### 5. Authorization before any device exists

The macOS Pier runs the same checks as the Linux Pier, in the same order, and
nothing is created until all pass:

1. `UsbHardDeviceMsg::report_descriptors()` (bounded hex decode);
2. `arcen_usb_bridge::authorize_hard_usb_tablet` — vendor-neutral
   classification as a pen tablet; joystick, gamepad and multi-axis
   collections refused; total descriptor bytes bounded. Vendor and product ids
   are logged, never trusted. The synthetic lab tablet needs
   `ARCEN_USB_HARD_SYNTHETIC=1`, as on Linux.
3. `BridgedDeviceGuard` on every descriptor the device then serves (§3).

**What is published differs from Linux, on purpose.** The Linux Pier bridges
a composite tablet's keyboard, keypad, boot-keyboard and mouse interfaces with
it (`tablet_claim.rs`), because it presents the whole device on a private seat
([ADR 0021](0021-bridged-usb-devices-on-their-own-seat.md)): those interfaces
reach that seat's session and nothing else. An `IOHIDUserDevice` has no seat.
It is system-wide: a keyboard interface would type into whatever holds the
console — another account's session after a Fast User Switch, or the login
window — and HID events bypass secure input. So a macOS Pier publishes only
what `arcen_usb_bridge::HidPublishPolicy::PenAndVendorOnly` allows
(`publishable`, shared and tested):

* every application collection is on the Digitizers page (`0x0D`) or a
  vendor-defined page (`0xFF00`-`0xFFFF`);
* every `Input` item's usages are digitizer, button or Generic Desktop axis
  usages (X, Y, Z, Rx, Ry, Rz, Slider, Dial, Wheel), or usages on the vendor
  page of the application collection they belong to (the tablet's own page)
  — never Keyboard/Keypad, Consumer, Telephony or Generic Desktop
  system-control usages, even inside a pen or vendor collection;
* Apple's own vendor pages that macOS's `IOHIDEventDriver` turns into events
  are refused anywhere (`APPLE_EVENT_PAGES`: `0xFF00` vendor messages and
  Touch Bar, `0xFF01` vendor keyboard — Spotlight, Launchpad, Fn,
  brightness, Mission Control —, `0xFF02` mouse, `0xFF0C` motion, `0xFF11`/
  `0xFF12` Touch Bar, `0xFF50` filtered events, `0xFF60` multitouch, `0x00FF`
  top case; values from Apple's open-source IOHIDFamily usage tables). Newer
  Apple pages whose values Apple no longer publishes are covered by the
  own-page rule, and the keyboard-like Apple pages act only on a device with
  `AppleVendorSupported`, a property the injector never sets;
* anything the bounded parser cannot read is withheld.

Withheld interfaces stay authorized (the guard holds the device to all of
them) and are enumerated with the device, but get no `IOHIDUserDevice` and
are never polled; the negotiation log lists them with the reason. A device
with nothing publishable is refused at the handshake. For an Intuos5 only the
pen interface (`0xFF0D`) is published; touch is withheld because Wacom uses
`0xFF00`, which is also Apple's vendor page, and the boot mouse because it is
a mouse. A tablet's express keys, where they are a keyboard interface, and
finger touch on such a tablet do not work on a macOS Pier.

### 6. Isolation and lifecycle

| Event | What happens |
| --- | --- |
| Session ends (Deck disconnects, network loss, Pier stop) | the session drops its `BridgedTablet`: outstanding URBs are cancelled, the child's input is closed, the child cancels its devices and exits; a child that does not exit in 500 ms is killed by pid |
| Agent exits or crashes (including `SIGKILL`) | the child's standard input reaches end-of-file; it exits; the kernel destroys every `IOHIDUserDevice` with the process that created it |
| Child crashes | its devices vanish with it; the session sees its output close and ends Native tablet for this attachment with a reason |
| Console leaves the account (Fast User Switch, return to the login window, log out) | devices are system-wide, so the session watches the console holder (`ConsoleWatch`, `console::console_holder`) while Native tablet is active: re-read every 100 ms and before any USB completion is forwarded (at most every 20 ms). The moment it is not this agent's uid, the device owner is **killed** (no queued report is delivered), its devices vanish, outstanding URBs are cancelled and the Deck gets `tablet_mode_result` "Native tablet (USB bridged) ended: … console …", `reconnect_required: true`. The session itself goes on (its desktop is in the background). An attachment never starts when the console is not this account's |
| Login window session | **refused**: the hello does not offer Native tablet (`usb_hard_v1: false`), and a Deck that asks anyway is refused with a reason. No bridged device is ever created at the login window |
| A fault in the adapter | every adapter step runs under `catch_unwind` (`contained`): a panic ends Native tablet with "an internal fault", never the session |
| Second Deck | a macOS Pier serves one session at a time; there is never more than one bridged tablet |

### 7. Vendor driver installed or not: what the Deck is told

> **Superseded 2026-11-12 (§14.2).** A macOS Pier now accepts only a model
> whose pen it reads itself and tells the Deck so; every other model is
> refused. The vendor-driver detection and the three notices below were
> removed with the half-working fallback they described. Kept as the record
> of what was measured.

| Host | Result |
| --- | --- |
| Tablet maker's driver installed and it supports the model | *intended*: the driver matches the device, switches it to tablet mode through a feature report (bridged as `SET_REPORT`) and produces pressure, tilt and buttons. **Not observed**: Wacom Tablet 6.4.9 on macOS 26.7.2, whose driver names the Intuos5 family, opened none of the three bridged devices (§13) |
| No vendor driver | macOS's generic HID driver binds what it understands: a standard digitizer collection gives position and proximity without pressure; a vendor-page interface (Wacom Intuos interface 0) gives nothing; a boot-mouse interface moves the pointer |

The Pier states this truthfully, and never with an empty "all is well". It
looks for an installed Wacom driver
(`/Library/LaunchAgents/com.wacom.wacomtablet.plist` or
`/Applications/Wacom Tablet.localized`), and an accepted
`tablet_mode_result.reason` always says what the pen will be:

| Driver | `reason` on an accepted Native tablet |
| --- | --- |
| none | applications see the pen as a pointer without pressure until the maker's driver is installed |
| installed | the maker's driver is here but not proven to accept a bridged tablet: the pen may be a pointer without pressure |
| unknown vendor | pressure needs a driver from the tablet's maker on this Mac |

The middle row exists because of the measurement in §13; it changes to a
promise only when a vendor driver is seen to bind a bridged device. The negotiation log line "Hard USB negotiation inputs" records
`usb_hard_available`, `vendor_driver`, and the refusal reason, as on Linux.
Detection is a hint for the message, never a gate: the devices are created
either way.

### 8. Negotiation

The hello advertises `usb_hard_v1: true` and
`tablet_mode_capabilities.usb_tablet_bridge: available` only when all hold:

| Condition | Refusal reason when false |
| --- | --- |
| built with `native-tablet` (default) | "this Pier build does not contain the Native tablet adapter" |
| not the login window | "Native tablet is not offered at the login window" |
| the injector program is installed | "the Pier executable that owns virtual HID devices is missing" |
| a probe child (`probe-virtual-hid`) created and cancelled a device | the probe's own refusal (entitlement or Accessibility) |

The probe runs as a child of the session process at startup (it settles in
24-40 ms, measured) and is re-run at most every 5 seconds after a refusal, so
a grant given later is seen by the next connection without a restart. A hello
never answers "still checking": it waits, bounded by 3 seconds, for a probe
that is still running (§8a). A probe child that does not answer in 10 seconds
is killed and counts as a refusal.

### 8a. Native tablet works on the first connection

Native tablet attaches during the first connection, without signing out,
signing in, reconnecting or restarting the desktop. On a macOS
Pier this holds by construction, and the implementation keeps it so:

1. **Devices appear while the session runs.** `IOHIDUserDevice` devices are
   hot-plugged into the running Aqua session: the HID system matches and
   publishes them within milliseconds of `IOHIDUserDeviceActivate`
   (measured: the devices published and the pen polled 0.4 s after the
   client hello, §13). Nothing about the desktop has to be restarted for them
   to be seen. Xorg on the Linux Pier hot-adds a bridged tablet too, once its
   X server has read Arcen's input class at start (a one-time display-manager
   restart after enabling Native tablet; [ADR 0021](0021-bridged-usb-devices-on-their-own-seat.md)).
2. **The same connection carries the device.** The Deck captures its tablet
   when the server hello offers Native tablet and names it in the client
   hello of that same connection; the Pier authorizes it and starts the
   attachment in that session. A refusal is the only path that answers
   `reconnect_required`.
3. **The first hello is truthful and early enough.** The capability probe
   starts with the agent (or `serve`); the hello waits for a probe still in
   flight instead of refusing, so a Deck arriving the instant the agent
   starts is still offered Native tablet. A refusal is re-probed for the next
   connection, so granting Accessibility later needs no restart.
4. **No per-user state is required first.** No driver install, system
   extension approval, reboot or prompt stands between the first connection
   and the devices: the entitlement is in the shipped signature and the
   Accessibility grant is the one the Agent Helper already holds for the
   keyboard. (A vendor driver, if it binds, is the host's own business and is
   not a precondition.)

After the client hello, the device is authorized (§5); a refusal sets
`tablet_mode_result.reason` to "Native tablet (USB bridged) refused: …" and
the active mode falls back exactly as `resolve_tablet_mode` decides. An
authorized device whose child fails to create its devices ends Native tablet
for the attachment with the child's reason; the session continues.

### 9. The child contract (`arcen_usb_bridge::hid_device_ipc`)

Length-prefixed binary frames (`kind u8`, `length u32` little-endian,
payload), each ≤ 32 KiB, defined and tested in shared code:

* parent → child: `Create` (one per interface: index, properties, report
  descriptor), `Activate` (after the last `Create`), `Report` (index, bytes),
  `ReportReply` (token, status, bytes);
* child → parent: `Ready` / `Refused` (reason), `GetReport` / `SetReport`
  (token, index, type, id, length/bytes).

A get/set report block blocks its device's dispatch queue until the parent
answers or 3 s pass, then returns `kIOReturnTimeout`. The child refuses a
frame that breaks the contract and exits, which tears every device down.

### 10. Tests

Automated, per layer:

* `shared/usb-bridge`: the enumeration sequence against the emulated Intuos5
  (`WacomIntuos5` through `SyntheticDevice`, no network): descriptors, plan,
  properties, polling, report requests (the mode switch reaches the device),
  every guard violation, every bound, timeouts, cancellation and
  unknown-completion failure; the IPC codec round-trips and rejects oversize
  and malformed frames.
* `hosts/macos`: availability and refusal reasons, login-window refusal, the
  vendor-driver message, wire translation of adapter requests, and the
  injector's frame loop with a fake device.

Live, with the Deck CLI built with `usb-emulate-lab` (`input-smoke
--tablet-mode hard --usb-emulate wacom-intuos5`) against a Pier built with
`native-tablet` (the build feature is the only switch on macOS) on a test Mac
with an installed package. Where no installed Pier is available, the ignored
end-to-end test stands in for the Deck process: the Deck's own `UsbExporter`
and emulated Intuos5, and the client hello a Deck sends:

| Proof | How |
| --- | --- |
| devices appear with VID/PID/descriptor/transport | `ioreg -r -c IOHIDUserDevice -l` and `hidutil list` during the session |
| reports arrive, at the expected rate | an `IOHIDManager` probe (lab subcommand) that opens the bridged pen interface, sends the Wacom mode switch through `IOHIDDeviceSetReport` (exercising the set-report path end to end) and counts input reports and their intervals |
| teardown | the same listings after the Deck disconnects, after `SIGKILL` of the session process, and after the child is killed |
| login window refusal | unit-tested; on a test Mac by connecting while the login window is up |
| pressure with the vendor driver | on a Mac with Wacom's driver: an AppKit probe window under the scripted stroke logs `NSEvent` pressure |

Wacom Tablet 6.4.9, whose driver names the Intuos5 family, did not bind the
bridged devices (§13); pressure therefore comes from the Pier's own pen
reader (§14.1), not from a vendor driver.

### 11. Security considerations

1. **A real vendor's identity on the host.** The device claims to be the
   Deck's tablet (`056a:0317` for an Intuos5). That is the point — the vendor
   driver must match — and it is a claim only an authenticated Deck can make,
   after the shared classifier and guard. macOS marks every device
   `HIDVirtualDevice` with `Transport` `"Virtual"` (measured, §13) and it
   carries a session-scoped serial.
2. **No keyboard, keypad or mouse is ever published** (§5): those
   interfaces are withheld on macOS, because a published device is
   system-wide and would act on another account's session or the login
   window. What is published dies the moment the console leaves the
   session's account (§6).
3. **Feature and output reports reach the physical device.** The host driver
   (or any local process that opens the device through `IOHIDManager`) can
   set and read reports; they travel to the Deck's tablet as HID class
   requests, which the Deck's exporter forwards. This is the same reach the
   Linux host kernel has. Standard requests other than those in §3 are never
   generated.
4. **The entitled child gains a mode.** `hid-injector --bridged-usb` creates
   devices from descriptors that came from the network. They are bounded and
   were authorized and guarded in the parent; the child re-checks the frame
   bounds, refuses any frame out of contract, and has no network, session or
   policy.
5. **Blocking callbacks.** A get/set report waits at most 3 s on a dedicated
   queue per device; a hostile or slow Deck can delay its own tablet only.
6. **Bulk-lane loss.** USB bridge records have their own relay queue
   (`RELAY_USB_BACKLOG`, 256, drained before clipboard records), so a
   clipboard transfer can never crowd them out; they are dropped only if that
   queue is full or the bulk lane is gone for 5 s (`usb_dropped`). A dropped
   control transfer is caught by the 3 s deadline, a dropped interrupt IN by
   the 10 s idle watchdog (§3).

### 12. Current behaviour

* **One entitled child.** Devices are created only by the existing
  `hid-injector` child in its `--bridged-usb` mode; the Agent Helper's
  signature carries no entitlement and cannot create devices.
* **Restricted interfaces.** Only the pen digitizer and vendor interfaces are
  published; keyboard, keypad and mouse interfaces are withheld (§5).
* **Known-model decoding.** The Pier reads the pen of a model it knows
  (§14.1) and refuses any other model with a reason; the Deck keeps Tablet
  support (§14.2).
* **Session-scoped serial numbers** replace the device's own serial number.
* **`Transport`** is requested as `"USB"`; macOS reports `"Virtual"` for an
  unprivileged creator (§13), which is harmless.
* **Teardown.** Devices disappear when the attachment ends, when the child or
  the session process dies, and when the console leaves the session's account
  (§6, §13).
* `IOUSBHostControllerInterface` (ADR 0019 §6) is not used by the Pier.

### 13. What was measured (macOS 26.7.2, 2026-11-10)

A Developer ID signed dev Pier app with the virtual HID entitlement, built
with `native-tablet`; the ignored end-to-end test
`a_bridged_tablet_is_published_as_hid_devices_fed_and_torn_down` (real Pier
stream and QUIC lanes, the Deck's `UsbExporter` serving the emulated Intuos5,
the real client hello, the entitled `hid-injector --bridged-usb` child) run
from a process whose responsible app holds Accessibility; `ioreg`, `hidutil
list` and `probe-bridged-hid` observed from outside.

| Proof | Result |
| --- | --- |
| first connection | the hello offered `usb_hard_v1` with the probe started the same instant; the tablet was accepted (`reconnect_required: false`) |
| devices | **one** `IOHIDUserDevice`, the pen (`0xFF0D`, 55-byte descriptor); touch (`0xFF00`) and the boot mouse withheld. `056a:0317`, version `0x0100`, "Wacom Co.,Ltd." / "Intuos5 touch L", session serials, `LocationID` `0xA2Cxxx00-01`; `Transport` reported as `"Virtual"` |
| enumeration to polling | 0.4 s from client hello |
| set/get report | Wacom mode switch `SET_REPORT(Feature 2) [2,2]` through `IOHIDDeviceSetReport`, read back with `GET_REPORT`: 0.7-1.0 ms round trip; the emulated tablet left mouse mode |
| reports | 800 pen reports delivered by the Deck side per run; an `IOHIDManager` reader saw 506-583 in its ~3.6 s window (138-162 Hz, mean 6.2-7.2 ms, max 6.8-10 ms) — the pace of the scripted pen, one report per poll plus 5 ms; loss was not measured |
| truthful result | accepted with "Native tablet is active. This Mac has the tablet maker's driver, but it is not proven to accept a bridged tablet: applications may see the pen as a pointer without pressure." |
| bindings | no Wacom process opened any device |
| teardown: disconnect | devices gone 14-23 ms after the session ended |
| teardown: child `SIGKILL` | devices gone at once; the Deck got `tablet_mode_result` "Native tablet (USB bridged) ended: the device owner exited", `reconnect_required: true`; the session streamed on |
| teardown: session `SIGKILL` | child exited on end-of-file; devices gone |
| without Accessibility | `probe-virtual-hid` refuses ("needs … Accessibility approval for the app that started it") and `serve` advertises Native tablet unavailable with that reason |
| console leaves the account | the e2e test hands the console away in-process (lab-only `set_console_holder_for_lab_tests`; the real reader is `console::console_holder`): to the login window and to another uid, the devices were gone within 31 ms by `probe-bridged-hid`, the Deck got the console reason with `reconnect_required: true`, and 32-33 video frames followed in the next 2 s. A real Fast User Switch was **not** performed (it needs a second account at the machine); `ConsoleWatch` and `SessionTablet`'s reaction are unit-tested with an injected console reader |
| login window session | unit-tested only |

### 14. Pressure: the host reads the pen; a DriverKit HID dext is not the route (2026-11-12)

Question: does a DriverKit HID dext (`IOUserHIDDevice`, the
`driverkit.family.hid.device` entitlement the profile carries) give a device
Wacom's driver or macOS turns into pressure? Evidence on a Mac with
macOS 26.7.2, Wacom Tablet 6.4.9, a real Intuos5 (`056a:0317`) attached:

| Finding | Evidence |
| --- | --- |
| A dext's HID device is not forced to `"Virtual"`: `AppleUserHIDDevice::newTransportString` returns what the dext's `newDeviceDescription` set, while `IOHIDUserDevice::newTransportString` returns `"Virtual"` unless the creator is privileged and sets `HIDVirtualDevice` | IOHIDFamily-2238.100.59 source (`AppleUserHIDDevice.cpp`, `IOHIDUserDevice.cpp`); SDK `IOUserHIDDevice.iig` lists `kIOHIDTransportKey`, vendor and product ids as settable |
| The real Intuos5's HID devices are themselves dext-backed (`AppleUserUSBHostHIDDevice`, class `AppleUserHIDDevice`, `Transport` `"USB"`), and Wacom reads the pen interface through an `IOHIDLibUserClient` on it | `ioreg` of the attached tablet |
| Wacom also opens an `AppleUSBHostDeviceUserClient` on *every* USB device (hubs, camera, LAN, security key): libusb-style enumeration. Its HID manager needs a `LocationID` and applies a filter whose criteria are not documented | `ioreg`; imported symbol listing of `WacomTabletDriver` |
| Wacom's own pressure reaches applications as CoreGraphics events: `com.wacom.IOManager` imports `CGEventPost`/`IOHIDPostEvent`; macOS has no generic HID-digitizer-to-pressure path (§13, `hosts/macos/AGENTS.md`) | imported symbols |
| CoreGraphics tablet events from the shared Intuos5 reader reach `AppKit` with pressure | `probe-pen --intuos5`: proximity (mask `0x25c7`), 46 tablet points, `NSEvent` pressure 0.059 → 1.000, tip and barrel edges |

So a dext could present `056a:0317` with `"USB"` and no virtual flag, but
whether Wacom then opens it is unknowable without building, signing,
installing and approving one — and it needs a new App ID, the
`com.apple.developer.driverkit` entitlement Apple grants on request, the
System Extension capability on the Pier, a dext that matches the existing
`IOHIDUserDevice` with `transport.hid` and re-publishes it (no user client,
so no `userclient-access` request), and an approval click on every Pier. The
host reading the pen itself needs none of that and is measured end to end on
the CoreGraphics side. The dext stays a researched fallback for vendor-driver
features (ExpressKeys, per-application settings).

#### 14.1 The macOS Pier interprets a known pen (2026-11-12)

Native tablet on a macOS Pier gives pressure by **reversing this ADR's
"never interpret" stance for the macOS Pier**, and only there: the Linux Pier keeps handing the device to its
kernel driver (ADR 0020, ADR 0021). It is a product path in 0.19.0, part of
Native tablet itself (no separate build feature):

| Rule | How |
| --- | --- |
| Only known models are read | `arcen_usb_bridge::host_decoded_intuos_model`: the vendor and product id must be in `HOST_DECODED_INTUOS_MODELS` (today `056a:0317`, Intuos5 touch L, whose ranges are Arcen's own emulation's); the pen interface is found by its declared input report (id 2, 10 bytes), not by number |
| Unknown models are refused | since §14.2; until then they were published as HID devices with the no-pressure notices of §7 |
| The mode switch | sent only to a known model, only inside an authorized attachment, after its devices are published: `GET_REPORT(Feature 2)` reads the mode, then `SET_REPORT(Feature 2) = [2, 2]`. The host's own requests use tokens the child can never use (a child request with one is refused) |
| Mode restore | when the attachment ends while the Deck is still connected (a fault, the console leaving the account, the device owner exiting) the host sets back the mode it read, before cancelling the rest, and leaves that request running; when the Deck disconnects no request can reach the device, and the Deck's hand-back (a held or plain USB reset, ADR 0019) returns the tablet to its power-on mode |
| Parsing | `decode_intuos_pen_report` reads only a report of at least the declared length with report id 2; every field is bounds-free arithmetic on fixed indices; samples are clamped into the wire ranges (`PenEventMsg::validate`) before injection; malformed and arbitrary bytes of every length 0-64 are unit-tested to never panic |
| One consumer | the pen interface's reports go to the reader, never also to the published device, so a driver that binds it one day cannot double the pen |
| What reaches applications | proximity (with the eraser as its own tool from the tool id), tip pressure (2048 levels), tilt, both barrel buttons; touch on the tablet surface and ExpressKeys do not (§5) |
| The cursor on the Deck | the pointer moves only on the host, so for a Deck that declared `cursor_position_v1` with a local cursor the single-display stream reports each pen position as a `CursorPosition` record (shared `HostPointerReporter`), and the Deck draws its cursor there until its own mouse moves; `cursor_mode_result.cursor_positions` says so. The daemon relays them onto the input/cursor lane through the shared `CursorPositionRelay` (newest wins, renumbered from 1 per lane); before this the split Pier's relay dropped every position record. Positions were chosen over a host-drawn cursor because they keep the Deck's cursor authority (no second cursor in the picture, the Deck's own mouse still leads when it moves) and need no capture reconfiguration. A multi-display session reports none yet |
| Console | a console change ends the attachment with no pen event, not even a release (§6); any other end lifts a pen left in proximity |

#### 14.2 One Native tablet way; unknown models are refused (2026-11-12)

One Native tablet way means the pen reader. A tablet whose
model the shared reader does not know (`host_decoded_intuos_model`) is no
longer published as HID devices for proximity and buttons without pressure;
`bridged_tablet::authorize` refuses it, and the Deck is told why with the
device's ids ("this Mac's Pier reads a tablet's pen itself and does not know
this model (vvvv:pppp) yet; use Tablet support"). The Deck keeps its tablet
and Tablet support works as before. Removed with it: the vendor-driver
detection (`vendor_driver_installed`, `WACOM_DRIVER_PATHS`) and the
`NO_VENDOR_DRIVER_NOTICE`, `UNPROVEN_VENDOR_DRIVER_NOTICE` and
`UNKNOWN_VENDOR_DRIVER_NOTICE` texts (§7). An accepted tablet always gets
`HOST_PEN_NOTICE`.

The `IOHIDUserDevice` publication (§1–§9) stays for an accepted model: the
adapter still enumerates the device through the child, the pen and vendor
interfaces are published (§5), and the pen interface's reports go only to
the reader (§14.1, "one consumer"). Whether a known model needs any published
device at all is a follow-up; removing the child would rewrite the adapter.

**Follow-up:** add models to the reader. Each needs its pen packet layout
and coordinate and pressure ranges established from an emulation or a
device the project has in hand (`HOST_DECODED_INTUOS_MODELS`), never
guessed.

The position of a pen this host reads is reported by the one path of
[ADR 0023](0023-one-native-tablet-pen-position-path.md): the reader's
device units become a shared `PenSample`, the shared lead maps it, and the
Deck draws the same pen dot as on Linux. The reader's fractions now use the
same scaling as libinput and the X server (`value / (max + 1)`), so the
injected event and the reported position land on the same pixel.

**Measured, 2026-11-12, lab macOS Pier (macOS 26.5.2, M2 Ultra), notarized
0.19.0 package, no Wacom driver on the host.** A Deck CLI on another Mac
played the emulated Intuos5 stroke (three strokes, 168 pen reports) through
Native tablet into the signed-in Aqua session, where an AppKit window proven
topmost on a 7×7 grid logged what arrived: 174 tablet points with `NSEvent`
pressure from 0.059 to 1.000 (17 levels), 6 proximity events, tip and
barrel buttons (`buttonMask` 0x1 | 0x2). The Pier read the mode (0) before
switching it (accepted), read 168 packets with none ignored, and sent 74
host pointer positions; the Deck received all 74 (first 0.277, 0.295, last
0.738, 0.688 of the stream). A real Intuos5 through a Deck was proven
later (0.19.0). Not measured here: the eraser end and tilt (the scripted stroke carries neither; both
are unit-tested from literal packets), and the mode restore (it runs only
when an attachment ends with the Deck still connected).

## Consequences

* A macOS Pier can be a Native tablet target with the entitlement it already
  has, through the Deck, wire and service that exist today.
* Pressure comes from the Pier's own reader for known models (§14.1) and
  needs no tablet maker's driver on the host; a model the reader does not
  know is refused, and the Deck keeps Tablet support (§14.2).
* Native tablet needs no sign-out, reconnect or restart on a macOS Pier (§8a).
* Only HID-class devices can ever be bridged this way. That matches the shared
  policy and is not a regression.
* `SIGNING.md`'s claim that a HID sink cannot carry the control transfers is
  corrected there.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| `IOUSBHostControllerInterface` (ADR 0019 §6) | needs a managed entitlement the Pier's profile does not carry, and a separate approval from Apple |
| Create the devices in the Agent Helper | unentitled signature; creation returns `NULL` |
| A DriverKit HID dext | a system extension that every Pier must install and approve, plus further entitlements (§14) |
| Typed pen samples (Tablet support) | already exists; it is a different mode, without the vendor driver's features |
| A new protocol carrying HID reports instead of URBs | would change the Deck and the wire for one host; the URB bridge already carries everything a HID interface needs |
