# ADR 0008: Virtual Display for Windows Hosts

**Status:** Superseded for product shipping (2026-08-11).
The source-only first-party experiment remains on disk, but Arcen does not
package, install, sign, service, or maintain a virtual-display driver.

[vdd]: https://github.com/VirtualDrivers/Virtual-Display-Driver

## Context

Pier-Windows serves a client by **mutating the physical display**: it changes
mode, and under `DisplayPolicy::ExactIsolated` makes the target the only active
output at (0,0). Because that is destructive, it writes a recovery journal
first, arms a watchdog, and restores on the way out.

That machinery (`display.rs`, `recovery.rs` and `nvapi.rs`) is the largest
part of the Windows Pier, and it is where the failures come from. A sign-out
once stranded a journal, and every later session was refused with an
instruction to run `arcen-pier restore-display`, a command that needs an
interactive desktop and so cannot run on a host reachable only through the
product that refused. Restoring automatically and setting an unapplicable
journal aside fixed that case, but the class of problem is structural: the
Pier breaks something global and promises to put it back.

An indirect display driver (IddCx) inverts that. The host presents a *virtual*
monitor at exactly the client's geometry and captures it. Nothing global is
mutated, so there is nothing to restore.

## Decision drivers

**A virtual display does not create sessions.** The one-interactive-desktop
limit on client SKUs is enforced by `termsrv.dll` as licensing policy; a display
driver creates monitors inside a session and cannot grant new ones. The session
takeover and the credential-provider scenario handling
remain necessary and unaffected. Multi-session means Windows Server with the RDS
role, or Azure Virtual Desktop.

**Comparable products have converged on IddCx.** Remote-workstation products
have moved from custom virtual display drivers to indirect display drivers, or
shipped one from the start; others ship no driver and ask headless users to
install one.

**Signing is the real cost, and Arcen already owes it.** An IddCx UMDF driver can be
*attestation* signed (EV certificate + Partner Center, 1–3 days) for Windows
10/11 desktop, but attestation is blocked on Windows Server, which needs WHQL.
`arcen-microphone` already needs that same pipeline and lists "protected
EV/Partner Center and WHCP/HLK signing" as remaining evidence. The pipeline is a
prerequisite either way and should be built once.

## Options

### 1. Fork VirtualDrivers/VDD and ship it

MIT licensed, no copyleft, already attestation-signed, headless-oriented.
Rejected as a *shipping* path for three reasons:

- **Provenance.** `arcen-microphone` is deliberately independently authored and
  states it does not derive from SysVAD, WDK samples, virtual cable projects or
  third-party drivers. Shipping a forked community driver would abandon the
  standard set for Arcen's only other driver, and would need clearance and an
  intake under `legal/ORIGINS.md`.
- **No dynamic control.** VDD reads its monitor count at startup and exposes no
  documented IOCTL for adding or removing a monitor at runtime. Arcen needs a
  monitor created per session at the client's geometry, which is exactly the
  capability VDD lacks.
- **RDP is its weak point and Arcen's main path.** VDD is a console-session IDD, not
  a remote-session driver (`IDDCX_ADAPTER_FLAGS_REMOTE_SESSION_DRIVER`), and
  session transitions are a reported instability trigger. Users arriving over
  RDP is the case this product must handle.

### 2. Write an original IddCx driver

Use only Microsoft's public WDK/IddCx headers and API documentation, with
original Arcen implementation. Do not copy Microsoft samples, community
drivers, proprietary SDK payloads, or another product's code. This gives a clean vendor
namespace, a bounded IOCTL interface designed for the LocalSystem broker, and
explicit render-adapter LUID selection.

### 3. Keep mutating the physical display

The status quo. Rejected as a destination, though it remains the shipping
behaviour until a driver exists.

## Superseding decision (2026-08-11)

Windows Pier consumes display targets owned by the installed Windows display
stack. Those targets may come from physical outputs, native NVIDIA
Quadro/datacenter/vGPU output slots, a hypervisor display device and its signed
guest driver, or an independently installed signed IddCx/indirect-display
driver. Arcen does not bundle, recommend, configure, update, or roll back an
external display driver.

For a supported NVIDIA adapter, Pier may provision an otherwise empty native
display ID by writing an Arcen EDID through NVAPI. This is not a new driver and
does not create a target from nothing: the NVIDIA driver must already enumerate
the display ID and one-bit output ID. Pier journals the complete pre-mutation
Windows topology and every original EDID, arms its out-of-process watchdog,
writes only the missing EDIDs, re-probes CCD/DXGI, and admits the request only
after the requested targets are active. The EDID changes then commit or roll
back with the physical output-provider transaction.

On a host with more than one adapter, one is selected as Arcen's sole
display/capture/NVENC adapter, and the transaction runs on that adapter.

### Native NVIDIA provisioning and its rollback

`platform.multi_monitor.nvidia_headless_enabled` unset lets startup enable
native NVIDIA provisioning for a GRID/Quadro-class adapter; `false` turns it
off and `true` forces it.

After a native display hotplug, CCD regenerates boot-local source,
clone-group and desktop-image identifiers, so generic topology recovery
alone can reject the regenerated topology or leave Arcen-written EDIDs
connected but inactive. The rollback therefore combines every Arcen-written
EDID with the physical topology journal and restores exact modes before it
removes the synthetic EDIDs. It removes the recovery journal only after it
has verified that:

- the original EDID and mode are back;
- every added display ID is disconnected with no EDID;
- the original active paths, geometry, rotation and primary output are back.

The Pier refuses three or more headless NVIDIA displays: applying that
topology can block `SetDisplayConfig` without a safe way to interrupt it.

The existing first-party IddCx source and provider contracts remain
non-product research evidence only. They are excluded from packages and do not
define the shipping architecture. Any future proposal to ship an Arcen display
driver requires a new ADR and explicit Release/Security approval.

This also means:

- Microsoft Basic Display Adapter cannot be used to invent an extra target; it
  can drive only paths exposed by the underlying hardware or hypervisor.
- RDP's Microsoft Remote Display Adapter belongs to an RDP session and is not a
  reusable display provider for Arcen's console/direct session.
- A headless Windows operator may separately use a reputable signed
  virtual-display driver, but its trust, Secure-Boot compatibility,
  servicing and recovery are outside Arcen.
- Native NVIDIA provisioning is default-off and requires exactly one explicit
  display/stream adapter. Capture and NVENC stay on that adapter; other GPUs
  remain outside Arcen's allowed set for application compute.

The remainder of this ADR is retained as historical rationale and records the
implemented source experiment; its original shipping decision below is no
longer active.

## Original decision (superseded)

**Adopt IddCx as the intended architecture for Windows display, and maintain an
original first-party driver.** Do not fork or install VDD as part of this
implementation. This decision is superseded by the 2026-08-11 decision above.

The first tranche now provides:

1. `arcen-iddcx-provider`, a safe Rust contract/model crate for exact
   capabilities, adapter affinity, EDIDs, modes, and one-to-four monitor
   requests;
2. an original UMDF 2.33 / IddCx 1.4 provider with handle-owned atomic
   replacement, rollback, cleanup rollback, dynamic monitor lifecycle, exact
   display configuration, and swapchain render-LUID verification;
3. an inherited broker-to-agent control-handle seam and an
   `arcen_outputs::OutputProvider` implementation. `platform.iddcx.enabled` is
   false by default and the offer
   is withheld unless the full driver capability and affinity gates pass; and
4. source-only manifests, portable lifecycle tests, and an unsigned WDK build
   path that never signs, installs, or deploys.

Shipping remains sequenced behind Release/Security-owned EV certificate,
Partner Center attestation, and the Windows Server WHQL/HLK decision. Physical
display mutation remains the default fallback while the IddCx gate is off.

## Consequences

- The display recovery journal, its watchdog, and the topology restore paths
  become a fallback rather than the primary path. `nvapi.rs` mode and timing
  fixups exist to make a *physical* vGPU head accept a client geometry, and lose
  most of their reason to exist.
- Arcen takes on a second driver, with the servicing, HVCI, Driver Verifier and
  rollback obligations `arcen-microphone` already documents.
- The control device remains SYSTEM/Administrators-only. The broker opens one
  inheritable handle and grants the session agent only that handle; the device
  ACL is not weakened for interactive users. Closing the last handle removes
  the complete virtual topology.
- Attestation-only signing would restrict virtual display to Windows 10/11
  desktop; Windows Server hosts would keep the physical-display path until WHQL.
- Nothing here changes account switching. The session work stands on its own.

## Open questions

Recorded because they are unverified, not because they are unimportant:

- Whether an IddCx monitor binds correctly on NVIDIA vGPU profiles, including
  compute-only profiles with display outputs disabled.
- Whether a console-session IDD survives an RDP connect and disconnect cleanly,
  or needs `IDDCX_ADAPTER_FLAGS_REMOTE_SESSION_DRIVER` handling.
- Whether WHQL-signed IddCx drivers load on all supported Server SKUs.
- Runtime capture/NVENC behavior on the intended NVIDIA vGPU profiles. Source
  and unsigned compilation do not replace signed lab installation evidence.
- Driver Verifier, HVCI, suspend/resume, GPU reset, and RDP transition evidence.
- Release signing, servicing, rollback, and Server WHQL/HLK approval. These are
  external requirements, not source-completeness claims.
