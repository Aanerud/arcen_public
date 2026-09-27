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

## HDR provider requirements

Wayland is also the boundary for future Linux desktop HDR. Selecting the Deck's
HDR preset must not be enough by itself. A native provider may retain PQ/HLG
only after it proves all of the following for the captured output:

- the compositor reports an active color-managed HDR output/image description;
- primaries and transfer characteristics are authoritative, not inferred from
  ten-bit storage;
- capture negotiates a format that preserves the compositor's HDR values
  (FP16 or a documented ten-bit format) rather than an 8-bit PipeWire fallback;
- SDR reference white and mastering/content-light metadata are available where
  the transfer requires them; and
- capture, input/EIS, resize, teardown, and reconnect remain bound to the same
  compositor output generation.

This future provider is a third Linux capture pipeline, not a replacement for
the two proven Xorg paths. Auto/Speed must continue to use NvFBC where its
device-to-device advantage is available, and Xorg Grading must continue to use
depth-30 XShm. Provider selection happens from explicit session/capability
truth; adding Wayland HDR must not route all sessions through PipeWire or add a
host copy to the eight-bit fast path.

Until those facts are implemented and measured, dedicated Xorg resolves HDR
requests to Grading Reference (HEVC 4:4:4 10-bit full-range BT.709). The Deck
shows matrix/primaries/transfer degradation and remains in SDR presentation
mode. Depth 30 proves precision, not HDR. The one exception is a desktop the
operator declares Rec.2100 PQ (`video.desktop_encoding`), where an
application such as Flame writes PQ itself; see `color-fidelity.md`.

`WaylandRuntimeFacts::from_process_environment` proves only the Wayland session
marker and Unix socket. It deliberately leaves protocol state unknown because
file presence or environment variables cannot prove registry, portal, or EIS
capability.

## Headless HDR spike result (2026-09-27)

A headless GNOME 50 session delivered a bit-exact PQ / BT.2020 ten-bit
desktop through PipeWire on the Linux lab (NVIDIA GRID V100D-16Q, driver
570.172.08, Rocky Linux 9.5 host, no display connectors). This is evidence
for the future provider, not a shipped pipeline.

Pipeline measured:

- **Session:** Mutter 50.5 (Fedora 44 container, podman with NVIDIA CDI)
  in headless mode on the NVIDIA render node through GBM, with one virtual
  monitor in `bt2100` colour mode (`gdctl set ... --color-mode bt2100`).
  The host needed `nvidia-drm modeset=1`; the Xorg pipelines were
  re-measured unchanged with it.
- **Content:** a Wayland client presenting an `XRGB2101010` shared-memory
  buffer tagged PQ / BT.2020 through `wp_color_management_v1`, carrying a
  0..1023 ramp.
- **Capture:** `org.gnome.Mutter.ScreenCast` `RecordMonitor`; the PipeWire
  stream advertises `xRGB_210LE`, `xBGR_210LE` and `RGBA_F16`, each tagged
  `colorPrimaries=BT2020` and `transferFunction=SMPTE2084`, as DMA-BUF and
  shared memory; a consumer asking for ten-bit PQ negotiated `xRGB_210LE`.
- **Result:** the captured frame held all 1024 codes, exactly, grey, and up
  to PQ 1023 (10,000 nits), so highlights above the 203-nit SDR white
  survive.

Stock Mutter 50.5 does not get there. Four changes were needed, each small,
and two of them are defects that label eight-bit pixels as ten-bit HDR:

1. Virtual outputs declare no colour spaces or HDR transfer functions, so a
   virtual monitor never offers `bt2100`. Declaring BT.2020 and PQ on them
   enables it.
2. A virtual monitor composites into a default eight-bit texture, where a KMS
   output prefers ten-bit scanout; it must composite into ten-bit.
3. The shared-memory screencast path paints every frame as eight-bit ARGB
   whatever format was negotiated, so an `xRGB_210LE` PQ stream carried
   256-level pixels. It must paint in the negotiated format.
4. The paint-to-buffer helper renders through a default eight-bit texture
   before reading back; it must render at the requested precision.

Arcen must not depend on a privately patched compositor: these belong
upstream, and the product capture should use DMA-BUF (the zero-copy route
into CUDA/NVENC), which does not pass through defects 3 and 4. KWin was not
chosen: its virtual backend sets no HDR capability and its HDR output
screencast change (plasma/kwin!9843) is closed unmerged. gamescope's
PipeWire output is eight-bit only.

### End to end to a Deck (2026-09-27)

A lab-only lever, `ARCEN_EXPERIMENTAL_RGB10_PIPE=<fifo>` (default off, a
systemd drop-in on the lab only), serves an HDR request from a FIFO that a
PipeWire consumer inside the container fills with tagged PQ / BT.2020 frames
(`capenc rgb10-pipe=`, `WideSource::ColorManagedPq`, READY capture backend
`pipewire`). A real Deck showed HDR: `rext 4:4:4 10-bit bt2020/pq/bt2020ncl`,
950-970 distinct luma codes, and 27.8-28 fps sustained for 120 s with no
dropped frames. Two defects found on the way are fixed: the pipe loop never
started the keyframe control thread (a late Deck waited forever), and the
consumer must open its FIFO before connecting the stream, not inside a
process callback. Input still reached Xorg, so this measures the provider's
video path only.

A host process can consume the container's PipeWire directly (host
libpipewire 1.0.1 against the container's 1.6.9 server over a shared runtime
directory), which is the route for in-process capture without a helper.

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
