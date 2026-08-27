# Linux Wayland output inventory boundary

**Status:** capability-gated model/interface tranche; no runtime Wayland or
libei support is claimed.

Linux Pier continues to select the dedicated-Xorg session model. The
default-off `wayland-provider` Cargo feature only makes a binary eligible to
consider the new host-local provider seams. It does not add a Wayland client,
D-Bus/portal client, Mutter adapter, libei adapter, or launcher selection path.

## Public host-local API

The public API is intentionally confined to `arcen-pier-linux`:

- `display::wayland::WaylandOutputSource` is the feature-gated future
  output-inventory seam. It reports a compositor snapshot; it does not own or
  mutate a display transaction.
- `display::wayland::WaylandOutputCapabilities` and
  `display::wayland::OutputCapabilityReport` contain compositor detection
  evidence. They are not the shared `arcen_outputs::OutputCapabilities`
  contract.
- `display::wayland::{WaylandOutput, WaylandOutputSnapshot}` combine coherent
  `wl_output` mode/scale/transform state with `xdg-output` logical regions.
- `display::wayland::detect_output_capability` evaluates compile-time and
  authoritative runtime facts. Unknown protocol state is unavailable.
- `input::eis::InputProvider` is the feature-gated future portal/libei seam.
- `input::eis::{EisRegion, EisRegionMap}` reconcile compositor-advertised EIS
  regions with current Arcen logical regions and map region-local coordinates.
- `input::eis::detect_input_capability` requires established output regions,
  RemoteDesktop portal availability, an EIS connection, and an absolute
  pointer capability.

These interfaces consume the existing pure `arcen-media` region value objects:
`RegionId`, `RegionGeneration`, `LogicalRect`, `PhysicalSize`, `Scale120`, and
`OutputTransform`. Native Wayland/EIS handles and provider traits do not enter
shared crates or the wire protocol.

## Geometry rules

- `xdg-output` position and size are the authoritative logical desktop region.
  They are converted from whole compositor logical pixels into Arcen's
  1/120-logical-pixel fixed-point domain.
- `wl_output` mode is retained as the explicit pre-transform physical extent.
- All eight `wl_output.transform` values map directly to the shared transform
  vocabulary.
- Integer `wl_output.scale` converts to `Scale120` by multiplying by 120.
- A fractional preference may override it only when a future provider can
  authoritatively associate the surface-scoped preference with that output.
  Merely observing `fractional-scale-v1` does not establish a global
  per-output scale.
- EIS regions use unsigned desktop-wide logical offsets. The pure mapper
  translates a Wayland layout's minimum signed origin to zero while preserving
  relative placement.
- EIS mapping IDs are preferred but not trusted as unique. During resize, an
  exact geometry match may disambiguate duplicate IDs; disagreement or
  ambiguity fails closed. Exact geometry is the fallback when IDs are absent.
- EIS physical scale and Wayland presentation scale are retained separately as
  `Scale120` metadata; neither is derived from or forced equal to the other.
  EIS physical scale does not alter absolute logical region coordinates.

## Capability gates

| Gate | Current result |
| --- | --- |
| Cargo feature absent | `FeatureDisabled` |
| Non-Linux target | `UnsupportedTarget` |
| Session/socket/core-protocol fact unknown | Typed unavailable reason |
| Missing `wl_output` or `xdg-output` | Typed unavailable reason |
| Native Wayland adapter | Not implemented |
| RemoteDesktop portal/EIS grant | Must be authoritatively supplied; no heuristic |
| Native libei adapter | Not implemented |
| Mutter virtual output | May report detected-but-unimplemented; never implemented |

`WaylandRuntimeFacts::from_process_environment` proves only the Wayland session
marker and Unix socket. It deliberately leaves protocol state unknown because
file presence or environment variables cannot prove registry, portal, or EIS
capability.

## Detection evidence and the later shared contract

The Wayland names describe inventory and detection only. A future native
adapter may use a successful `WaylandOutputSource` snapshot as evidence while
implementing the separately reviewed shared
`arcen_outputs::OutputProvider`; it must not pass
`WaylandOutputCapabilities` directly to the shared admission gate.

| Wayland evidence | Later `arcen_outputs::OutputCapabilities` meaning |
| --- | --- |
| `enumerate_outputs` | Selection precondition only; it is not a shared capability. |
| `xdg_output_logical_regions` plus a coherent snapshot | Permits `signed_desktop_coordinates` only when the provider verifies that logical placement is authoritative. |
| `fractional_scale` plus an authoritative output association | Permits `fractional_scale`; merely observing `fractional-scale-v1` is insufficient. |
| `mutter_virtual_output: Implemented` | May support `surface: Virtual` and `headless_capable: true`; the native provider must still prove the lifecycle and teardown promises. |
| Unknown, unavailable, or detected-but-unimplemented virtual-output state | No shared capability and no provider selection. |

The snapshot's mode, transform, scale, region, and teardown evidence may later
support the remaining semantic fields (`exact_modes`, `per_region_rotation`,
`persistent_dedicated_desktop`, and rollback), but those are provider promises
that require native implementation and verification. They are not inferred
from detection flags alone.

## Follow-up boundary

Runtime enablement requires separately reviewed native adapters and Linux
integration evidence. Any new third-party Wayland, D-Bus/portal, Mutter, or
libei dependency requires dependency and Release/Security review; launcher or
shared API selection changes also require Linux Host and Shared/Architecture
review. Until then, Xorg remains the production default and Wayland detection
must return an unavailable reason.
