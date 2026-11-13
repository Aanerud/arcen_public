# ADR 0020: Native tablet on Linux uses the distribution's in-tree `vhci-hcd`

**Status:** Accepted (2026-11-09); implemented, and a product feature since
0.19.0 (status note below). The package dependency's review is recorded
below. Builds on
[ADR 0015](0015-native-wayland-desktops-on-their-own-seat.md) (native Wayland
seats) and amends [ADR 0012](0012-hard-usb-on-windows-hosts.md)'s description of
the Linux importer.

**0.19.0 status note (2026-11-12):** Native tablet is a
product feature of the public Linux Pier. Every build carries it (Cargo
feature `native-tablet`, default), and it is an administrator opt-in at run
time: `usb.native_tablet = "enabled"` in `pier.json`, set by
`install-arcen-pier --native-tablet`. Without it nothing is offered and
`vhci-hcd` is never loaded. The installer installs `kernel-modules-internal`
only with Native tablet enabled (and `--no-native-tablet` removes a package
it installed); it explains the package's origin and support status as
decided below.

**Amended 2026-11-12 (one Native tablet way).** The old names are gone, not
deprecated: the `usb-hard-lab` Cargo feature (Pier, macOS Pier and Deck; the
lab-only `usb-emulate-lab` Deck feature stays as test tooling) and the 0.18
`ARCEN_USB_HARD_LAB=1` service environment. A host that relied on the
variable must set `usb.native_tablet` before upgrading.
Where the pen points is reported by the one path of
[ADR 0023](0023-one-native-tablet-pen-position-path.md).

## Context

Native tablet presents the Deck's physical tablet on a virtual USB host
controller, so the host kernel's own driver (`usbhid`, `wacom`, `hid-uclogic`,
…) binds it exactly as it binds real hardware. Until now the Linux Pier used
`usb-vhci`, an out-of-tree module with `/dev/usb-vhci` ioctls. It has to be built
for every kernel and, under Secure Boot, signed with a machine owner key the
operator enrols. The lab did exactly that, and it is not something a product
can ask of an administrator.

The alternatives were measured on Rocky Linux 9.7 (kernel 5.14.0-611.55.1) and
10.2 (6.12.0-211.66.1):

- `uhid` with the in-kernel `wacom` driver: `wacom` refuses 19 Wacom models,
  including the Intuos5 (056a:0317), unless they arrive through `usbhid`, and
  `hid-uclogic` refuses every non-USB device.
- `dummy_hcd` and raw-gadget: not built by either kernel.
- **USB/IP `vhci-hcd`**: built by both kernels (`CONFIG_USBIP_VHCI_HCD=m`, one
  controller, 8 USB 2 and 8 USB 3 ports), and a real host controller. Rocky
  ships it **only in `kernel-modules-internal`**, in the distribution's `devel`
  repository, one package per kernel release. Red Hat labels that package
  internal and unsupported. Its modules are signed by the distribution's kernel
  key (on the labs: `Rocky Linux kernel signing key`, the same key as `xfs`), so
  Secure Boot is untouched and the kernel taint is unchanged.

## Decision

1. **The in-tree `vhci-hcd` is the primary backend.** The privileged helper
   reserves a free USB 2 port (host-wide lock in `/run/arcen-usb-bridge/` and
   the kernel's `status` table), installs the per-port seat rule, then writes
   `"<port> <fd> <devid> <speed>"` to
   `/sys/devices/platform/vhci_hcd.0/attach`, where `<fd>` is one end of an
   `AF_UNIX`/`SOCK_STREAM` socketpair. It plays the device on the other end with
   the USB/IP protocol (`CMD_SUBMIT`/`RET_SUBMIT`/`CMD_UNLINK`/`RET_UNLINK`;
   codec in `arcen_usb_bridge::usbip`, bounded, no isochronous transfers).
   Teardown writes the port to `detach`; a helper that dies closes the socket,
   and the kernel disconnects the port by itself. The Deck, the wire and the
   descriptor guard are unchanged: URBs map onto the same bridge URB lane.
2. **`usb-vhci` stays only as a lab fallback**, chosen automatically when
   `vhci-hcd` cannot be loaded and `/dev/usb-vhci` exists. Arcen never builds,
   signs or installs it. Without either, Native tablet is refused, and the
   reason names `kernel-modules-internal` and the exact `dnf` command.
3. **The installer installs `kernel-modules-internal` on Rocky/RHEL-family
   hosts** for the exact running kernel, unless `--no-native-tablet-modules` is
   given:
   - `dnf --enablerepo=devel install kernel-modules-internal-$(uname -r)`, with
     `gpgcheck` and `localpkg_gpgcheck` on. For a Rocky kernel whose point
     release has moved to the vault, the exact package URL in Rocky's `devel`
     tree or vault, still GPG checked.
   - It verifies that `vhci-hcd`'s signer equals the signer of the kernel's own
     modules, and removes the package again if it does not.
   - It loads nothing itself. The Pier runs `modprobe vhci-hcd` once, when it
     starts (connections only check that it is loaded, so a Pier already
     running when the package arrives needs a restart or a manual
     `modprobe vhci-hcd`); no `modules-load.d` entry is written.
   - It records the package in
     `/var/lib/arcen/installer-installed-kernel-modules-internal`. `--uninstall`
     removes only recorded packages, before `--purge` removes the record.
   - On other distributions it says where the module would come from and
     leaves Native tablet unavailable.
4. **Kernel updates.** The package follows one kernel release, and dnf does
   not install it for a new kernel. The Pier logs the mismatch at start, and
   `validate-config` and the Native tablet refusal name the exact command. No
   dnf plugin is shipped; the administration guide documents the step.
5. **Seat isolation is unchanged in principle:** the per-port rule puts the
   whole port subtree on the session's own seat before the port is attached. On
   a native-seat Wayland desktop (ADR 0015) that is the desktop's own
   `seat-arcen-pen-<id>`, and Mutter hot-adds the tablet through logind; the
   rule also removes `uaccess`. Headless Wayland desktops stay refused.

## Release/Security notes

- The package comes from the distribution, signed with its RPM key, and its
  modules with its kernel key. Arcen ships no kernel code and never touches the
  machine owner key list or Secure Boot.
- `kernel-modules-internal` also carries the distribution's kernel self-test
  modules. Installing them loads none of them; only `vhci-hcd` and its
  dependency `usbip-core` are ever loaded, by the Pier.
- Red Hat labels the package internal and unsupported. A host administrator
  who needs a supported configuration can opt out with
  `--no-native-tablet-modules`; Tablet support is unaffected.
- The `attach` and `detach` files are root-only (0200); only the root helper
  writes them. The USB/IP codec is written from the public protocol
  description (`legal/ORIGINS.md`).

## Consequences

- No self-built or self-signed module is needed on Rocky Linux or RHEL.
- An administrator must reinstall the matching package after a kernel update
  for Native tablet to keep working; Arcen says exactly what to run.
- Every Pier on a host, and any operator use of `usbip`, shares the one
  `vhci_hcd.0` controller: at most 8 bridged USB 2 devices per host.
