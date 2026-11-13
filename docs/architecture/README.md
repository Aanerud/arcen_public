# Arcen Architecture

> **STATUS — target vs. current.** This document describes the *target*
> design. **Current reality:** all eleven shared crates are live:
> `arcen-protocol`, `arcen-transport`, `arcen-media`, `arcen-input`,
> `arcen-outputs`, `arcen-session`, `arcen-identity`, `arcen-keel`,
> `arcen-telemetry`, `arcen-observability` and the safe Hard USB policy/state
> core `arcen-usb-bridge`.
> `arcen-transport`'s opt-in QUIC feature is active in the Linux, Windows and
> macOS Piers and the macOS Deck while its default dependency graph stays
> Quinn-free. Shipped transport is
> **direct QUIC only**, using the shared TLS trust lifecycle. Dormant WSS source
> is isolated behind `wss-compat` and is not enabled by product builds.
> Current status is the repository `README.md`; see
> [`observability.md`](observability.md) for the direct Pier/Deck observability
> implementation. This is not a release claim.

Arcen is divided into reusable shared crates and independently deployable
products. Dependency direction is one way: products consume shared crates;
shared crates never consume products; products never consume one another.

## Product topology

- **Hosts:** Linux (X11, and GNOME/Mutter Wayland) and Windows workstation
  Piers; a macOS Pier shipped as a preview
- **Clients:** macOS desktop application
- **Deferred:** Linux and Windows clients

The macOS Pier is a shared-first host with native boot/login feasibility gates
and signed-package acceptance criteria ([`hosts/macos/AGENTS.md`](../../hosts/macos/AGENTS.md)).
The logged-in Aqua host path exists, but this is not a release claim: cold-boot LoginWindow operation is still target work, and
multi-monitor is not qualified on two-display hardware.

## Transport direction

Direct products use QUIC (Quinn + rustls TLS 1.3). Since 0.17.0 one
authenticated connection carries isolated lanes — control, video, audio out,
microphone, input/cursor and bulk — each on its own stream with its own worker, declared
by a bounded lane preface (protocol v5,
[ADR 0014](../adr/0014-quic-lanes-and-capture-contracts.md)). WSS network
code is dormant behind `wss-compat` and absent from product feature graphs.
The products do not use QUIC datagrams. See
[`transport.md`](transport.md) for API usage and known gaps, and
[`../adr/0007-quic-only-product-transport.md`](../adr/0007-quic-only-product-transport.md)
for the current decision. Transport mechanics remain behind `arcen-transport`;
protocol compatibility remains in `arcen-protocol`.

## Identity and machine authentication

User identity and OS session authentication establish who may use the host.
The current direct Piers use their platform authentication paths; generic OIDC
with an initial federated-identity profile remains future architecture. Host machine
authentication identifies the workstation independently of the user identity
plane. Commercial product-entitlement enforcement was withdrawn when Arcen moved
to AGPL-3.0 free software; see [ADR 0006](../adr/0006-offline-pier-licensing.md).

Conflating identity and host/session authorization would make authorization
ambiguous and couple product availability to operating-system implementation
details.

## Windows virtual-output boundary

Windows Pier consumes targets owned by the installed display stack. They may be
physical, supplied by a hypervisor and its signed guest driver, created by a
separately installed signed indirect/IddCx display driver, or pre-existing spare
display IDs exposed by a supported NVIDIA Quadro/datacenter/vGPU adapter.
For the NVIDIA case Pier can write a journalled EDID to the spare native output,
re-probe CCD/DXGI, and include it in the ordinary physical output-provider
transaction. Arcen does not package, install, configure, service, or roll back a
Windows virtual-display driver.

The existing `arcen-iddcx-provider` and
`hosts/windows/driver/arcen-iddcx` source remain dormant research evidence
only. They are excluded from product packages, and `platform.iddcx.enabled`
must remain false on supported installations. Any future proposal to ship an
Arcen driver requires a new architecture decision and Release/Security review.
See the superseding decision in
[`ADR 0008`](../adr/0008-virtual-display-for-windows-hosts.md).

## Shared boundaries

| Crate | Responsibility |
| --- | --- |
| `arcen-protocol` | Explicit version negotiation, bounded JSON control messages, protocol-v3 binary headers, endpoint capability exchange, and foundation-only additive `multi_monitor_v1` negotiation metadata |
| `arcen-keel` | Pure 16×16 content-damage tracking and deterministic scenario corpus; consumed by Windows software selective conversion |
| `arcen-transport` | Active shared rustls posture, certificate lifecycle, pins, reload, and transport contracts; opt-in Quinn/rustls direct QUIC carrier plus the separately unused advanced stream/datagram adapter |
| `arcen-usb-bridge` | Active safe, OS-free exact-profile policy, descriptor parsing, attachment lifecycle, URB ledger, synthetic lab tablet, and the device-exporter engine (`UsbExporter` over a platform `CapturedUsbDevice` backend, ADR 0019) for the default-off Hard USB vertical slice |
| `arcen-identity` | Provider-neutral OIDC/session-grant contracts plus bounded standalone disclaimer validation and acceptance evidence |
| `arcen-session` | Active pure session policy: restore leases, direct reconnect, Deskside, admission, Pier configuration schema, connection diagnostics, polkit locality and the Wayland provider's qualification and session policy — see [`shared/session/ARCHITECTURE.md`](../../shared/session/ARCHITECTURE.md) |
| `arcen-outputs` | Monitor lifecycle, multi-display arrangement and admission, EDID construction ([ADR 0010](../adr/0010-shared-output-provider-lifecycle.md)) |
| `arcen-media` | Active pure clipboard policy/sequence/echo/repeated-copy, bounded PNG/DIBV5 conversion, codec/chroma truth, fixed audio policy, checked video planes/conversion, media-plan resolution, optional source-built software H.264, validated monitor topology, foundation-only multi-monitor topology/admission contracts, the Deck platform profile (`deck_platform`: Apple silicon vs Intel decisions over architecture and macOS-version facts), and the colourist's Colour check and test-pattern measurement |
| `arcen-input` | Active pure absolute/relative pointer, cursor-authority, global sequence ordering, typed pen/tablet (`PenEvent`/`PenTool`) capability/event contracts, keyboard policy, committed text and Paste as Keystrokes, host cursor images, and the one shared region-input encode/decode (`RegionInputWireMessage`), host pipeline (`RegionInputPipeline`), and client emitter (`RegionInputEmitter`) — see [`pen-tablet-input.md`](pen-tablet-input.md) |
| `arcen-telemetry` | Pure correlation IDs, stable lifecycle categories, and redaction-safe fields |
| [`arcen-observability`](observability.md) | Active OS-free tracing and I/O abstractions for canonical routing, live profile reload, and bounded sink workers |

Who uses each shared crate, through which separate product path, and what each
path must pass when that crate changes is the generated
[shared consumer register](shared-consumers.md), built from
`shared/<crate>/consumers.json`, `scripts/ci/validation-profiles.toml` and
`cargo metadata` by `scripts/check_shared_contracts.py`.

## Documents in this directory

| Document | Covers |
| --- | --- |
| [`LEXICON.md`](LEXICON.md) | The canonical vocabulary |
| [`transport.md`](transport.md) | Direct QUIC: carrier, lanes, congestion control, known gaps |
| [`tls-certificate-lifecycle.md`](tls-certificate-lifecycle.md) | Certificate generation, validation, pinning and reload |
| [`session-auto-reconnect.md`](session-auto-reconnect.md) | Credential-free resume after a dropped connection |
| [`media-plan-resolution.md`](media-plan-resolution.md) | How a requested preset becomes a served pipeline; portable software H.264 |
| [`color-fidelity.md`](color-fidelity.md) | Ten-bit, 4:4:4, full range, Grading and HDR from capture to the Deck |
| [`encode-intent.md`](encode-intent.md) | Interactive versus grading encoder intent |
| [`rate-control.md`](rate-control.md) | Live bitrate control |
| [`qp-maps.md`](qp-maps.md) | Keel's damage grid as NVENC QP maps |
| [`arcen-keel.md`](arcen-keel.md) | Keel, the content-adaptive engine (planning document) |
| [`nvenc-sdk13-blackwell.md`](nvenc-sdk13-blackwell.md) | NVENC SDK 13 on Blackwell: 4:2:2 and Ultra High Quality |
| [`codec-royalties.md`](codec-royalties.md) | What the public record says about codec royalties and AV1 |
| [`audio-compression.md`](audio-compression.md), [`audio-input.md`](audio-input.md) | Host audio out and the Deck's microphone in |
| [`pen-tablet-input.md`](pen-tablet-input.md) | Typed pen and tablet input (local termination) |
| [`linux-wayland-provider.md`](linux-wayland-provider.md) | The Linux Wayland provider, with lab evidence; qualification records in [`wayland-qualification/`](wayland-qualification) |
| [`windows-console-policy.md`](windows-console-policy.md) | What the Windows Pier does with the physical console |
| [`timezone-redirection.md`](timezone-redirection.md) | The session following the Deck's time zone |
| [`disclaimer-banner.md`](disclaimer-banner.md) | The pre-session notice a host can require |
| [`macos-peripheral-access.md`](macos-peripheral-access.md) | Webcams and other peripherals from a Mac: which route |
| [`observability.md`](observability.md), [`ObservabilityStandard.md`](ObservabilityStandard.md) | Logging, QoS telemetry and monitoring |
| [`shared-consumers.md`](shared-consumers.md) | Generated register of which product path uses each shared crate |

Decisions are in [`../adr/`](../adr); running and configuring a Pier is in
[`../operations/`](../operations).
