# Arcen Architecture

> **STATUS — target vs. current.** This document describes the *target*
> design. **Current reality:** the live shared crates are `arcen-protocol`,
> `arcen-input`, `arcen-media`, `arcen-keel`, `arcen-telemetry`,
> `arcen-observability`, `arcen-identity`, the default restore-lease surface
> of `arcen-session`, and the dependency-light TLS lifecycle default of
> `arcen-transport`, plus the safe Hard USB policy/state core
> `arcen-usb-bridge`.
> `arcen-transport`'s opt-in QUIC feature is active in the direct Linux/Windows
> Pier and macOS Deck products while its default dependency graph stays
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

- **Hosts:** Linux and Windows workstation agents
- **Clients:** macOS desktop application
- **Deferred:** macOS host, Linux and Windows clients

## Transport direction

Direct products use QUIC (Quinn + rustls TLS 1.3) on one bidirectional stream
carrying the existing authenticated, WebSocket-framed session protocol. WSS
network code is dormant behind `wss-compat` and absent from product feature
graphs. This baseline does not yet split control/media into
independent streams or use datagrams. Those mechanisms remain available for a
future reviewed optimization; see
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
| `arcen-usb-bridge` | Active safe, OS-free exact-profile policy, descriptor parsing, attachment lifecycle, URB ledger, and synthetic lab tablet for the default-off Hard USB vertical slice |
| `arcen-identity` | Provider-neutral OIDC/session-grant contracts plus bounded standalone disclaimer validation and acceptance evidence |
| `arcen-session` | Active default pure IANA/restore-lease, direct-reconnect, and Deskside decisions |
| `arcen-media` | Active pure clipboard policy/sequence/echo, bounded PNG/DIBV5 conversion, codec/chroma truth, fixed audio policy, checked video planes/conversion, media-plan resolution, optional source-built software H.264, validated monitor topology, and foundation-only multi-monitor topology/admission contracts |
| `arcen-input` | Active pure absolute/relative pointer, cursor-authority, global sequence ordering, typed pen/tablet (`PenEvent`/`PenTool`) capability/event contracts, and the one shared region-input encode/decode (`RegionInputWireMessage`), host pipeline (`RegionInputPipeline`), and client emitter (`RegionInputEmitter`) — see [`pen-tablet-input.md`](pen-tablet-input.md) |
| `arcen-telemetry` | Pure correlation IDs, stable lifecycle categories, and redaction-safe fields |
| [`arcen-observability`](observability.md) | Active OS-free tracing and I/O abstractions for canonical routing, live profile reload, and bounded sink workers |
