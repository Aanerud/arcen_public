# macOS Pier: App ID, signing, notarization and entitlements

This records what the macOS Pier is signed as, how it is notarized, and —
mostly — which entitlements it deliberately does **not** have.

## The App ID and bundle identifiers

| | |
| --- | --- |
| Team ID | `NWR7ZH8L7U` (Tuttifrutti AS) |
| App ID description | Arcen Pier |
| Bundle ID | **`pier.arcen.tech`** |

Reverse-DNS, so the domain `arcen.tech` becomes `tech.arcen`, then the product.
Writing it the other way round — `pier.arcen.tech` — is a natural slip and
produces an App ID that matches nothing Arcen ships, because the identifier in
the signed bundles is `pier.arcen.tech`. If a profile ever fails to apply, check
this first.

Two bundles are signed, with separate identifiers:

| Bundle | Identifier | Installed at |
| --- | --- | --- |
| Arcen Pier | `pier.arcen.tech` | `/Applications/Arcen Pier.app` |
| Arcen Agent Helper | `pier.arcen.tech.agent` | `/Applications/Arcen Agent Helper.app` |

The helper is a **sibling**, not nested inside the Pier bundle. This is not
tidiness. TCC attributes a permission request to the bundle it can find by
walking up from the executable, so a helper inside `Arcen Pier.app` gets its
consent credited to the Pier: the dialog says the wrong name and Privacy &
Security lists the wrong application. That was observed, not theorised. A
top-level bundle has no container to be credited to.

## Signing and notarization

```sh
packaging/macos/build-pier-pkg.sh \
  --identity "Developer ID Application: Tuttifrutti AS (NWR7ZH8L7U)" \
  --installer-identity "Developer ID Installer: Tuttifrutti AS (NWR7ZH8L7U)" \
  --notary-profile <notary-keychain-profile-name>
```

Three distinct credentials, and they are not interchangeable:

| Credential | Signs | Note |
| --- | --- | --- |
| Developer ID **Application** | the executables and both bundles | |
| Developer ID **Installer** | the `.pkg` | A different certificate type. "3rd Party Mac Developer Installer" is the Mac App Store one and cannot sign a package for distribution outside it. |
| Notary keychain profile | — | Stored by `xcrun notarytool store-credentials`; found under keychain service `com.apple.gke.notary.tool`. |

Both the application bundle **and** the package are notarized and stapled.
Gatekeeper assesses the package when it is opened and the application when it
is launched, so notarizing only the payload leaves a package refused before its
contents are ever examined.

`build-pier-app.sh` builds the release `arcen-pier-macos` binary before it
assembles the bundle. That is intentional. An earlier version merely checked
that `target/release/arcen-pier-macos` existed and packaged it, which produced
a signed, notarized, successfully installed package containing a binary older
than the source fix being tested.

Signing is inside out and never uses `codesign --deep`: `--deep` re-signs
nested content with the outer identity, which would give the helper the Pier's
identifier and undo the separation above.

A **fourth** credential exists but is not part of a normal build: the
provisioning profile for `pier.arcen.tech`. It is signing material and is
never committed — `.gitignore` excludes `*.provisionprofile` repository-wide,
unanchored so that a profile dropped in a directory nobody anticipated is still
excluded. Keep it outside the tree (alongside the other Apple credentials) and
pass its path when it is needed. It is only needed for
`--with-driverkit-hid`, described below.

## Entitlements: none in the shipping build, and that is the correct answer

The Pier and its helper ship with **no entitlements**. A provisioning profile
authorizing three DriverKit HID entitlements now exists (see below), and the
build can embed it on request, but the default build does not, because nothing
in the Pier uses them yet.

This is a deliberate position rather than an omission, and it is worth
understanding before adding any.

On a Developer-ID-distributed, unsandboxed macOS application, the things this
host does are governed by **TCC consent**, not by entitlements:

| Capability | What actually gates it |
| --- | --- |
| Screen capture (ScreenCaptureKit) | Screen Recording consent. No entitlement exists or is needed. |
| Input injection (CGEvent) | Accessibility consent. No entitlement. |
| Clipboard (NSPasteboard) | Nothing. |
| System audio capture (Core Audio process taps) | The same Screen & System Audio Recording consent as capture, plus `NSAudioCaptureUsageDescription` in the **Info.plist** — a usage string, not an entitlement. |
| Local authentication (PAM) | Ordinary process privileges. |
| QUIC listener | Nothing; the process is not sandboxed. |

Requesting an entitlement the provisioning profile does not authorize is
actively harmful: AMFI terminates the process before `main`, which the Deck
already experienced and recorded in
`clients/macos/APPLE_ENTITLEMENT_REQUESTS.md`. Notarization succeeding does not
prevent it — those are separate gates.

The Hardened Runtime *is* enabled (`codesign --options runtime`), because
notarization requires it. That is a signing option, not an entitlement.

### Specifically considered and rejected

**`com.apple.developer.sustained-execution`** — asks the system to *limit burst
performance* so an app sees a consistent, sustainable performance level. That
is the opposite of what this host wants: video capture and encode benefit from
exactly the short-term boost it suppresses, and the Pier has no need to measure
its own steady-state throughput. Adding it would make encoding slower for no
gain.

### The profile that exists, and what it does not buy

A Developer ID provisioning profile for the App ID `pier.arcen.tech` exists and
authorizes exactly three entitlements. Two others that appear in Apple's
documentation — `com.apple.developer.hid.virtual.device` and
`com.apple.developer.driverkit.transport.usb` — were **not offered in the
portal** for this App ID, so the profile could not carry them.

| Entitlement | What Apple documents it as | macOS |
| --- | --- | --- |
| `com.apple.developer.driverkit.family.hid.device` | "whether the driver provides a HID-related service to the system" | 10.15+ |
| `com.apple.developer.driverkit.family.hid.eventservice` | "whether the driver provides a HID-related event service to the system" | 10.15+ |
| `com.apple.developer.driverkit.transport.hid` | "whether the driver communicates with human interface devices" | 10.15+ |

Read the subject of those sentences: **the driver**. These authorize a DriverKit
extension, not an application. An app that *installs* a dext needs
`com.apple.developer.system-extension.install`, which this profile does not
carry. The Pier ships no dext, so signing the Pier with these grants it nothing
it can use.

They are not free, either. Build the Pier with
`build-pier-app.sh --with-driverkit-hid --provisioning-profile FILE` and the
profile is validated and embedded; the script refuses the flag without a
profile, because that combination produces a binary that cannot start.

### The USB import path needs a different entitlement entirely

Established from Apple's SDK headers and entitlement documentation, not from
memory. The Pier's job is to receive a tablet's traffic over the network and
make a device appear to macOS applications. There are two ways to do that, and
**the three entitlements above authorize neither.**

Apple's "Human Interface Device Drivers" group has five keys. The distinction
that matters is what the device is attached to:

| Entitlement | Covers | Held |
| --- | --- | --- |
| `com.apple.developer.hid.virtual.device` | "the driver **creates a virtual HID device**" | no |
| `com.apple.developer.driverkit.family.hid.virtual.device` | "lets an app **create and manage virtual HID devices**" | no |
| `com.apple.developer.driverkit.transport.hid` | "permission to **interact with the hardware**" | yes |
| `com.apple.developer.driverkit.family.hid.device` | the driver **provides a HID service** | yes |
| `com.apple.developer.driverkit.family.hid.eventservice` | the driver **provides a HID event service** | yes |

`transport.hid` is permission to talk to *hardware*. This host has none — the
traffic arrives over a QUIC socket. The two entitlements that describe a device
with nothing behind it are the *virtual* ones, and the profile carries neither.

**The simple path does not need DriverKit at all.** `IOHIDUserDevice` is a
public, linkable IOKit API for exactly this:

* Header `IOKit.framework/Headers/hidsystem/IOHIDUserDevice.h`, in the public
  module map at `module.modulemap:83-95`.
* `IOHIDUserDeviceCreateWithProperties`, `IOHIDUserDeviceActivate` and
  `IOHIDUserDeviceHandleReportWithTimeStamp` are all in the `IOKit.tbd` link
  stub. macOS 10.15+.
* The report descriptor is `CFData` under `kIOHIDReportDescriptorKey` in the
  creation dictionary (`IOHIDUserDevice.h:127-130`).
* It runs in an **ordinary user-space process**. No dext, no system extension,
  no approval prompt, no reboot.
* It requires `com.apple.developer.hid.virtual.device` **and that alone**,
  which the header itself states at lines 118-121.

So the action is narrow: obtain `com.apple.developer.hid.virtual.device` for
`NWR7ZH8L7U.pier.arcen.tech` and regenerate the profile. It was not offered in
the portal for this App ID, which means asking Apple rather than ticking a box.
Until it exists, the importer cannot be built — not because the code is hard,
but because AMFI kills a process that requests an unauthorized entitlement
before `main`.

The DriverKit alternative is strictly worse here: it needs
`com.apple.developer.driverkit` (which Apple's own page says **must be
requested from Apple**), `com.apple.developer.system-extension.install` on the
containing app, and `driverkit.family.hid.virtual.device` — three further
grants, a system extension install flow, and a user approval step, to reach the
same place.

### Entitlements and the profile are a pair, and a dev Mac hides it

Measured, not assumed. The same Developer-ID-signed binary, on a machine with
no provisioning profiles installed:

| Build | Result |
| --- | --- |
| Entitlements, profile embedded | runs |
| Entitlements, no profile | **SIGKILL — exit 137, no output, before `main`** |
| No entitlements | runs |

There is no error message. The process is killed by the kernel, so a wrapper
sees a signal, not a diagnostic, and `codesign --verify` and notarization both
pass on the binary that dies.

The trap is that this is invisible on a development Mac. On a workstation with
profiles installed system-wide, the unprofiled build **ran fine** — the
installed profiles satisfied the check on that machine and nowhere else. It was
only running the identical bundle on a clean machine (zero profiles in
`/var/db/MobileDevice/ProvisioningProfiles` and in the Xcode user store) that
exposed it. Any conclusion about entitlements drawn on a machine that has ever
had Xcode profiles installed is worthless.

Profile file permissions are *not* part of this: AMFI reads
`Contents/embedded.provisionprofile` in kernel context, and a root-owned,
mode-600 profile that the running user cannot read still launches normally. The
600 the build produces matches the shipping Deck and is correct.

### Basic Tablet needs none of this

The typed pen sink does not require a driver, a dext, or an entitlement.
CoreGraphics carries tablet data on ordinary mouse events: the subtypes
`kCGEventMouseSubtypeTabletPoint` and `kCGEventMouseSubtypeTabletProximity`,
with `kCGTabletEventPointPressure`, `kCGTabletEventTiltX`/`TiltY`,
`kCGTabletEventRotation`, `kCGTabletEventTangentialPressure`, and the
proximity fields including `kCGTabletProximityEventEnterProximity` and
`kCGTabletProximityEventPointerType`. Apple's own wording is that these values
are set with `setIntegerValueField(_:value:)`.

Native Tablet — where this Mac *publishes* a device so the host's own Wacom
driver binds it — is the only thing that could need DriverKit. That is the
opposite operation from the Deck, which *takes* a physical tablet away from
macOS and forwards its traffic; the Deck's requirements are recorded separately
in `clients/macos/APPLE_ENTITLEMENT_REQUESTS.md` and do not apply here.

**The open question is now answered, by observation.** An Intuos5 touch L
(`056a:0317`, reported as `VendorID 1386` / `ProductID 791`) attached to a Mac
appears in `ioreg -c IOHIDDevice` with:

```
"Transport"      = "USB"
"IOUserClasses"  = ("AppleUserUSBHostHIDDevice", "IOUserUSBHostHIDDevice",
                    "IOUserHIDDevice", "IOHIDDevice", ...)
```

macOS binds the tablet with **Apple's generic USB-HID driver**, in userspace.
There is no Wacom-specific kernel driver in the chain for base tablet function
on a Mac without Wacom's driver installed.

That is an observation about how a tablet *plugged into a Mac* enumerates. It
is **not** a conclusion about Native Tablet, and an earlier version of this
file drew one anyway: it claimed a virtual HID device would therefore suffice.
It would not. Native Tablet is defined by what the Deck sends — the Deck's
helper takes the device away from macOS and forwards its **raw USB traffic**,
untouched, so that the host's own Wacom driver claims it exactly as if it were
plugged in there. The mode's whole value is that Arcen never interprets the
device. A HID-only sink would have to parse USB transfers to recover reports,
which is interpretation, and it has nowhere to carry the control transfers a
Wacom tablet needs to leave generic-mouse mode and report full pressure.

So the requirement is the one the Deck's own settings state: a host that can
**present the tablet on a virtual USB controller**. macOS has a public API for
exactly that, and it is the one this file twice described wrongly.

### `IOUSBHostControllerInterface`, and two corrections

`IOUSBHost.framework` ships `IOUSBHostControllerInterface.h` in the public SDK.
Apple's own description:

> IOUSBHostControllerInterface enables a process to instantiate a USB host
> controller to provide access to **remote USB devices** or create synthetic USB
> devices. The entitlement `com.apple.developer.usb.host-controller-interface`
> is required to use this class.

"Provide access to remote USB devices" is Native Tablet, stated in Apple's
words. The mode is available on macOS in principle, and the missing piece is
one named entitlement rather than an architectural gap.

Two claims in earlier versions of this file were wrong, in opposite directions,
and both are worth keeping visible because the second was the more damaging:

1. The entitlement identifier was first *asserted* without being checked.
2. It was then "corrected" to **does not exist**, on the evidence that
   `developer.apple.com` returns 404 for it. That inference was wrong. The
   entitlement is real and is named verbatim in Apple's SDK header; it is
   simply absent from the documentation website.

The lesson is not "check the docs". It is that **absence from the documentation
website is not evidence of absence**. The SDK headers are authoritative and
local, and grepping them takes seconds. A missing doc page had been treated as
proof, and a real capability was written off because of it.

`com.apple.developer.usb.host-controller-interface` is not on this profile and
was not offered in the portal for this App ID, so it has to be requested from
Apple. That request is the prerequisite for Native Tablet on macOS — not a
DriverKit dext, and not any of the HID entitlements the profile carries.

Basic Tablet — the mode the Deck recommends for WAN, where this host receives
finished pen samples and injects them — needs none of this: no driver, no
extension, no entitlement.

For completeness, the four HID entitlements differ by grammatical subject,
which is easy to read past:

| Entitlement | Apple's subject | On our profile |
| --- | --- | --- |
| `com.apple.developer.driverkit.family.hid.virtual.device` | "lets **an app** create and manage virtual HID devices" | **no** |
| `com.apple.developer.driverkit.family.hid.device` | "whether **the driver** provides a HID-related service" | yes |
| `com.apple.developer.driverkit.family.hid.eventservice` | "whether **the driver** provides a HID-related event service" | yes |
| `com.apple.developer.driverkit.transport.hid` | "whether **the driver** communicates with human interface devices" | yes |

The three we hold describe a driver. None of them provides USB virtualization;
`com.apple.developer.usb.host-controller-interface` does, and is described
above.

DriverKit also implies a System Extension, which is a separate capability with
its own provisioning profile; the Deck's profile was deliberately trimmed of
System Extension and DriverKit and that trimming should not be undone here by
accident.

`com.apple.vm.device-access` is closed to Developer ID distribution; Apple DTS
confirmed this (Case-ID 21584866, recorded in the Deck's entitlement notes). Do
not re-request it.

## Permissions have to be asked for, not just checked

TCC lists an application in Privacy & Security only once it has **requested**
access. A host that only preflights is invisible there: the operator opens
Screen Recording, finds an empty list, and reasonably concludes the install
failed.

`arcen-pier-macos request-permissions` calls `CGRequestScreenCaptureAccess`,
which registers the bundle and shows the consent dialog.

**Where the request comes from decides what gets registered.** TCC attributes a
request to the process that makes it, so asking from a terminal — over SSH, or
from an installer script — registers whatever that terminal's responsible
process is, not this bundle. An installed helper can therefore hold its port,
authenticate a real Deck, and still be absent from the Privacy list with
nothing for an operator to switch on. The agent asks for itself at startup, and
that is what puts these rows in the database:

```
kTCCServiceScreenCapture  | pier.arcen.tech.agent | 0
kTCCServiceAccessibility  | pier.arcen.tech.agent | 0
```

Zero is "not granted", and that is the point: the subject exists and can be
approved.

### One grant covers audio, and a tap lies about it

There is **no separate toggle for system audio**. Apple's pane is called
"Screen & System Audio Recording" because the same consent covers both, and
measurement on a machine where it is denied shows why that matters:

| | Screen Recording denied | granted |
| --- | --- | --- |
| tap created | yes | yes |
| format reported | 48 kHz stereo float32 | 48 kHz stereo float32 |
| mute requested and honoured | yes | yes |
| **callbacks** | **0** | dozens per second |
| **`audio_observed`** | **false** | true |

Everything a caller might check for success succeeds, and no audio arrives.
This is exactly the failure `AudioProbeReport::audio_observed` exists to catch:
a permitted-but-silent capture is not a working one, and treating tap creation
as evidence is how a host ends up advertising audio nobody can hear. Read
`audio_observed` from `probe-audio`; never infer audio from a tap that
constructed.

Granting cannot be automated, by design, and should not be attempted. The
installer's job is to make sure the operator is asked at a sensible moment and
told plainly what happens if they decline: the host will accept a connection,
authenticate the user, and then refuse to capture.

## If a permission ever appears under the wrong name

Check, in this order:

1. Is the helper still a top-level bundle, or has it been nested again?
2. Did something re-sign with `--deep` and overwrite the helper's identifier?
3. Is the `LaunchAgent` running the helper's executable, or the Pier's?

All three produce the same symptom: consent attributed to `pier.arcen.tech`
when it should belong to `pier.arcen.tech.agent`.
