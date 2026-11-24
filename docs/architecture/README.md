# Arcen Architecture

Arcen is divided into reusable shared crates and independently deployable
products. Dependency direction is one way: products consume shared crates;
shared crates never consume products; products never consume one another.

## Product topology

- **Piers (hosts):** Linux (X11, and GNOME/Mutter Wayland), Windows, and
  macOS as a preview.
- **Decks (clients):** macOS and Windows. A Linux Deck does not exist yet.

The macOS Pier serves a logged-in Aqua desktop. Cold-boot LoginWindow
operation and multi-monitor operation on two-display hardware are not yet
qualified, so it is a preview rather than a supported host. Its pipeline is
in [`hosts/macos/STREAMING.md`](../../hosts/macos/STREAMING.md).

Twelve shared crates carry the portable logic; the products own only what an
operating system forces on them. The repository `README.md` gives the current
status of each product.

## Transport

Every product speaks direct QUIC (Quinn and rustls, TLS 1.3) on UDP 18444,
with the shared TLS trust lifecycle. One authenticated connection carries
isolated lanes — control, video, audio out, microphone, input and cursor, and
bulk — each on its own stream with its own worker, declared by a bounded lane
preface (protocol v5,
[ADR 0014](../adr/0014-quic-lanes-and-capture-contracts.md)). No product
carries WebSocket code, and none uses QUIC datagrams.
[`transport.md`](transport.md) covers the API and known gaps;
[ADR 0007](../adr/0007-quic-only-product-transport.md) records the decision.
Transport mechanics stay behind `arcen-transport`; protocol compatibility stays
in `arcen-protocol`. `arcen-transport`'s QUIC runtime is an opt-in feature, so
its default dependency graph stays free of Quinn and Tokio.

## Identity and machine authentication

User identity and OS session authentication establish who may use the host.
The Piers use their platform's own authentication; generic OIDC with a
federated-identity profile is future work. Host machine
authentication identifies the workstation independently of the user identity
plane. Arcen has no licence or entitlement enforcement; see
[ADR 0006](../adr/0006-offline-pier-licensing.md).

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

The `arcen-iddcx-provider` and `hosts/windows/driver/arcen-iddcx` sources are
dormant research only. They are excluded from product packages, and
`platform.iddcx.enabled` stays false on supported installations. Any future proposal to ship an
Arcen driver requires a new architecture decision and Release/Security review.
See the superseding decision in
[`ADR 0008`](../adr/0008-virtual-display-for-windows-hosts.md).

## Shared boundaries

Twelve shared crates, none of which calls the operating system:

| Crate | Responsibility |
| --- | --- |
| `arcen-protocol` | Explicit version negotiation, bounded JSON control messages, binary frame headers, endpoint capability exchange, and `multi_monitor_v1` negotiation metadata |
| `arcen-keel` | Pure 16×16 content-damage tracking and deterministic scenario corpus; consumed by Windows software selective conversion |
| `arcen-transport` | Shared rustls posture, certificate lifecycle, pins, reload and transport contracts; the opt-in Quinn/rustls direct QUIC carrier, and an advanced stream/datagram adapter no product uses |
| `arcen-usb-bridge` | Safe, OS-free exact-profile policy, descriptor parsing, attachment lifecycle, URB ledger, a synthetic test tablet, and the device-exporter engine (`UsbExporter` over a platform `CapturedUsbDevice` backend, ADR 0019) for Hard USB, which is off by default |
| `arcen-identity` | Provider-neutral OIDC/session-grant contracts plus bounded standalone disclaimer validation and acceptance evidence |
| `arcen-session` | Pure session policy: restore leases, direct reconnect, Deskside, admission, Pier configuration schema, connection diagnostics, polkit locality and the Wayland provider's qualification and session policy — see [`shared/session/ARCHITECTURE.md`](../../shared/session/ARCHITECTURE.md) |
| `arcen-outputs` | Monitor lifecycle, multi-display arrangement and admission, EDID construction ([ADR 0010](../adr/0010-shared-output-provider-lifecycle.md)) |
| `arcen-media` | Pure clipboard policy/sequence/echo/repeated-copy, bounded PNG/DIBV5 conversion, codec/chroma truth, fixed audio policy, checked video planes/conversion, media-plan resolution, optional source-built software H.264, validated monitor topology, multi-monitor topology and admission contracts, the Deck platform profile (`deck_platform`: Apple silicon vs Intel decisions over architecture and macOS-version facts), and the colourist's Colour check and test-pattern measurement |
| `arcen-input` | Pure absolute/relative pointer, cursor-authority, global sequence ordering, typed pen/tablet (`PenEvent`/`PenTool`) capability/event contracts, keyboard policy, committed text and Paste as Keystrokes, host cursor images, and the one shared region-input encode/decode (`RegionInputWireMessage`), host pipeline (`RegionInputPipeline`), and client emitter (`RegionInputEmitter`) — see [`pen-tablet-input.md`](pen-tablet-input.md) |
| `arcen-telemetry` | Pure correlation IDs, stable lifecycle categories, and redaction-safe fields |
| [`arcen-observability`](observability.md) | OS-free tracing and I/O abstractions for canonical routing, live profile reload, and bounded sink workers |
| `arcen-deck-core` | The portable Deck: the platform seams (`DeckServices`, `VideoDecoderBackend`, `VideoPresenterSink`), settings and, behind features, the session runtime and shared screens |

Who uses each shared crate, through which separate product path, and what each
path must pass when that crate changes is the generated
[shared consumer register](shared-consumers.md), built from
`shared/<crate>/consumers.json`, `scripts/ci/validation-profiles.toml` and
`cargo metadata` by `scripts/check_shared_contracts.py`.

Every difference between products must be earned: an operating-system API or
behaviour forces it. The generated
[earned-differences ledger](platform-differences.md) lists each earned
difference with its OS reason and code, and each open gap with its owner;
`scripts/check_shared_contracts.py` fails when the code and the ledger
disagree or cross-product copies grow. The Shared Adoption Index trend is in
[`shared-adoption-history.md`](shared-adoption-history.md).

## Documents in this directory

| Document | Covers |
| --- | --- |
| [`transport.md`](transport.md) | Direct QUIC: carrier, lanes, congestion control, known gaps |
| [`tls-certificate-lifecycle.md`](tls-certificate-lifecycle.md) | Certificate generation, validation, pinning and reload |
| [`session-auto-reconnect.md`](session-auto-reconnect.md) | Credential-free resume after a dropped connection |
| [`host-presence.md`](host-presence.md) | The status line on Deck Home and a future presence service |
| [`media-plan-resolution.md`](media-plan-resolution.md) | How a requested preset becomes a served pipeline; portable software H.264 |
| [`color-fidelity.md`](color-fidelity.md) | Ten-bit, 4:4:4, full range, Grading and HDR from capture to the Deck |
| [`encode-intent.md`](encode-intent.md) | Interactive versus grading encoder intent |
| [`deck-presentation-timing.md`](deck-presentation-timing.md) | When each Deck shows a decoded frame, per pipeline; pixel-perfect fullscreen |
| [`rate-control.md`](rate-control.md) | Live bitrate control |
| [`qp-maps.md`](qp-maps.md) | Keel's damage grid as NVENC QP maps |
| [`arcen-keel.md`](arcen-keel.md) | Keel, the content-adaptive engine |
| [`nvenc-sdk13-blackwell.md`](nvenc-sdk13-blackwell.md) | NVENC SDK 13 on Blackwell: 4:2:2 and Ultra High Quality |
| [`audio-compression.md`](audio-compression.md), [`audio-input.md`](audio-input.md) | Host audio out and the Deck's microphone in |
| [`pen-tablet-input.md`](pen-tablet-input.md) | Typed pen and tablet input (local termination) |
| [`linux-wayland-provider.md`](linux-wayland-provider.md) | The Linux Wayland provider: desktops, capture, input and host qualification |
| [`windows-console-policy.md`](windows-console-policy.md) | What the Windows Pier does with the physical console |
| [`display-lifecycle.md`](display-lifecycle.md) | What a session leaves behind: displays removed or restored, stranded windows moved back |
| [`timezone-redirection.md`](timezone-redirection.md) | The session following the Deck's time zone |
| [`disclaimer-banner.md`](disclaimer-banner.md) | The pre-session notice a host can require |
| [`macos-peripheral-access.md`](macos-peripheral-access.md) | Webcams and other peripherals from a Mac: which route |
| [`observability.md`](observability.md), [`ObservabilityStandard.md`](ObservabilityStandard.md) | Logging, QoS telemetry and monitoring |
| [`shared-consumers.md`](shared-consumers.md) | Generated register of which product path uses each shared crate |
| [`platform-differences.md`](platform-differences.md) | Generated ledger of earned differences between products, open gaps, the capability matrix and the clone baseline |
| [`shared-adoption-history.md`](shared-adoption-history.md) | Shared Adoption Index per product over time |

Decisions are in [`../adr/`](../adr); running and configuring a Pier is in
[`../operations/`](../operations).
