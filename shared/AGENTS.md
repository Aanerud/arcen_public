# Shared/Architecture Ownership

**Owner role:** Shared/Architecture

This role owns all shared APIs, dependency direction, protocol compatibility,
transport abstractions, identity boundaries, entitlement interfaces, session
models, and reusable test support under `shared/`.

**Current state.** `shared/protocol` (`arcen-protocol`),
`shared/input` (`arcen-input`), `shared/media` (`arcen-media`),
`shared/keel` (`arcen-keel`), `shared/telemetry` (`arcen-telemetry`),
`shared/observability` (`arcen-observability`), `shared/identity`
(`arcen-identity`), the default
`shared/session` (`arcen-session`) surface, and
`shared/transport` (`arcen-transport`, dependency-light TLS core by default),
`shared/usb-bridge` (`arcen-usb-bridge`, safe Hard USB policy/state core),
and `shared/deck-core` (`arcen-deck-core`, the portable Deck, ADR 0024)
are live and in workspace `members`.
`arcen-observability` is the OS-free
tracing and bounded-I/O runtime; `arcen-telemetry` remains pure.
See `docs/adr/0004-platform-scope.md` and `docs/architecture/transport.md`.

Validate with `python3 -m unittest scripts/test_validate_observability.py`,
`cargo fmt --all --check`,
`cargo test --locked -p arcen-deck-core -p arcen-identity -p arcen-input -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge -p arcen-cp-ipc -p arcen-keel`,
`cargo clippy --locked -p arcen-deck-core -p arcen-identity -p arcen-input -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge -p arcen-cp-ipc -p arcen-keel -- -D warnings`,
the same test and strict-Clippy gates for `arcen-deck-core` with
`--features runtime,egui,wgpu-presenter`,
the `arcen-media/software-h264-source` test and strict-Clippy gates on Rust
1.89+ with C++ and NASM,
`cargo test --locked -p arcen-transport --features quic`, the test and
strict-Clippy gates for `arcen-transport --features issuer` (the shared
certificate issuer), and the CI
`cargo tree --locked -p arcen-transport -e normal` proof that the default graph
contains no `quinn`, `tokio`, `tokio-util`, or `bytes` package line.
CI likewise proves the default `arcen-media` graph excludes OpenH264 and its
native build dependencies.
The matching local `arcen-outputs` purity proof is:
```sh
product_package_pattern="$(
  cargo metadata --locked --no-deps --format-version 1 |
    python3 -c '
import json
import re
import sys

metadata = json.load(sys.stdin)
product_roots = {"hosts", "clients", "packaging"}
names = sorted(
    {
        package["name"]
        for package in metadata["packages"]
        if product_roots.intersection(
            package["manifest_path"].replace("\\", "/").split("/")[:-1]
        )
    }
)
print("|".join(re.escape(name) for name in names))
'
)"
forbidden_pattern='(^|[[:space:]])(tokio|tokio-util|quinn|bytes|futures|async-trait) v'
if [ -n "$product_package_pattern" ]; then
  forbidden_pattern="$forbidden_pattern|(^|[[:space:]])($product_package_pattern) v"
fi
if cargo tree --locked -p arcen-outputs -e all | grep -E "$forbidden_pattern"; then
  echo "arcen-outputs unexpectedly includes a runtime or product dependency" >&2
  exit 1
fi
```
The `arcen-deck-core` purity proof is the same script with
`-p arcen-deck-core` and a pattern that also forbids the UI and GPU stack:
`(tokio|tokio-util|quinn|quinn-proto|bytes|egui|egui-wgpu|epaint|eframe|wgpu|wgpu-core|wgpu-hal|winit)`.
There is no single-platform `--workspace` build. `arcen-protocol`,
`arcen-input`, and `arcen-transport` retain crate-level `unsafe_code = "forbid"`;
`arcen-media` is crate-wide `unsafe_code = "deny"` with a single reviewed
OpenH264 raw `SetOption`/`GetOption` exception in `software_h264.rs`;
`arcen-observability` is crate-wide `unsafe_code = "deny"` with a single
reviewed exception, the Windows support-bundle file-identity adapter
(`support_bundle/native_identity/windows.rs`: read-only `FileIdInfo`,
volume-flag, remote-protocol and final-path queries). Shared manifests must
not reference `hosts/`, `clients/`, or `packaging/`.

Shared code behaves the same on every OS. An OS-conditional `cfg` in shared
production code (`target_os`, `windows`, `unix`, `target_family`…) needs a
`cfg_sites` entry on a row of `scripts/ci/platform-differences.toml` naming
the OS API or behaviour that forces it (or the gap it is), with the reviewed
count per enclosing function; `scripts/check_shared_contracts.py` fails
otherwise. See [`docs/architecture/platform-differences.md`](../docs/architecture/platform-differences.md).

Escalate public API, wire compatibility, trust-boundary, cryptography, and new
third-party dependency changes to Release/Security. Consult the maintainers
of affected consumers before changing shared behavior.

## Native tablet: what lives here (reuse before adding)

| Need | Use | Crate |
| --- | --- | --- |
| A pen's position from its own reports | `tablet_pointer::{PenSample, EvdevPenFrames, PenPointerLead, PenPointerHub}` | `arcen-input` |
| Whether a host remapped the tablet, and the scaling | `tablet_pointer::{PenMapping, x11_wacom_mapping, map_to_output}` | `arcen-input` |
| Telling the Deck where the host pointer is | `host_pointer::{HostPointerFeed, HostPointerReporter, HostPointerLayout}` | `arcen-input` |
| The Deck's lead decision and the pen dot | `host_pointer::{DeckHostPointer, PointerAuthority, led_pointer_mark}` | `arcen-input` |
| The wire record and its relay | `cursor_position::{CursorPosition, CursorPositionRelay}` | `arcen-protocol` |
| Is this device a tablet; may it be bridged | `tablet_claim`, `bridged_device_guard` | `arcen-usb-bridge` |
| Udev rule and Xorg classes for a bridged device | `bridge_seat` | `arcen-usb-bridge` |
| Reading any tablet's pen (macOS and Windows Piers): generic reader, dedicated readers, the choice | `generic_pen::{GenericPenLayout, GenericPenReader}`, `wacom_intuos_pen::{IntuosPenReader, HOST_DECODED_INTUOS_MODELS}`, `host_pen::host_pen_route` | `arcen-usb-bridge` |
| One attachment on a Pier that reads the pen itself: USB host driver, reader, Intuos mode switch and restore, proximity release (`HidPublishPolicy::Nothing` for a Pier that publishes no device) | `host_pen_bridge::HostPenBridge` | `arcen-usb-bridge` |
| The same as wire frames and pen events, authorization, the Deck's tablet-mode wording, the host pointer the pen moves | `bridged_pen::{authorize, tablet_result, NativeTabletAttachment, BridgedPenSession, host_pen_event, HostPenPointer}` | `arcen-input` |
| Which Native tablet route a session takes (host driver or host decoded) | `joined_pen::{native_tablet_route, JoinedPenRefusal}` | `arcen-usb-bridge` |
| The proof that a joined desktop's typed input devices (keyboard, pointers, pen) reach it alone, and what a session does when it lapses | `joined_input::{prove_joined_device_confinement, classify_reader, JoinedDeviceFacts, JoinedInputGuard}` | `arcen-input` |
| What a host-decoded pen claims (decoded within carried) and the Deck's wording | `bridged_pen::{host_decoded_capabilities, host_decoded_notice, host_decoded_tablet_result}` | `arcen-input` |
| The bridge engine | `exporter` (Deck), `hid_host` (macOS and Windows Piers) | `arcen-usb-bridge` |
| The Deck's USB component status and its wording | `native_component::{NativeUsbComponentState, NativeComponentFacts}` | `arcen-usb-bridge` |
| Framing between a Deck and its USB component (macOS helper, Windows service) | `component_ipc` | `arcen-usb-bridge` |
| Serving one Deck connection from a component | `component_session::{serve_session, UrbCodec}` | `arcen-usb-bridge` |
| How long a Deck may hold the tablet (console session/user lease, revocation) | `component_lease::LeaseAuthority`, `component_session::SessionRevocation` | `arcen-usb-bridge` |
| The Windows component's WinUSB package, claim journal and release/recovery plans; its service/pipe names; which attached tablet is prepared | `winusb_package` (`TabletPreparation`) | `arcen-usb-bridge` |
| The Deck's client of its USB component (handshake, URB relay, cancel-safe completions) over any stream | `runtime::usb_component::ComponentClient` | `arcen-deck-core` |
| The settings screen's Native tablet row (state, action) | `DeckServices::{native_usb_component_state, run_native_usb_component_action}` | `arcen-deck-core` |

The full map, with the platform adapters, is in
[`docs/architecture/pen-tablet-input.md`](../docs/architecture/pen-tablet-input.md#native-tablet-where-each-piece-lives).

## Native tablet: one path

`arcen_input::tablet_pointer` is the one pen-position path for every Pier
(ADR 0023): `PenSample` (what a platform adapter produces), `EvdevPenFrames`
(evdev → samples, `SYN_DROPPED` resync), `PenPointerLead`/`PenPointerHub`
(pen leads in proximity under a whole-area mapping, otherwise the
platform's pointer), `x11_wacom_mapping` (the remap check), and
`map_to_output` (the X server's and Mutter's scaling). It feeds
`host_pointer::HostPointerFeed`; the Deck's mark is
`host_pointer::led_pointer_mark` (a bridged pen always gets the pen dot).
Platform code only reads devices and publishes; never add mapping or lead
logic under `hosts/`.

## The Deck core (`arcen-deck-core`)

The portable half of every Deck (ADR 0024). A platform shell
implements three seams and nothing else crosses the boundary:
`DeckServices` (clipboard, decode capabilities, microphone, network facts,
typed pen capture, build identity, config/log folders, displays and colour
facts, time zone), `decode::VideoDecoderBackend` + `VideoDecoderFactory`
(native picture as an associated type; HEVC RExt 4:4:4 10-bit, P010/Y410;
frame-exact pictures for Exact still image fences) and
`present::VideoPresenterSink` (eight-bit and ten-bit dedicated presenters,
the canvas executor `execute` with `CANVAS_EXECUTOR`, ADR 0028: a sink that
states none never makes its Deck advertise `canvas_v1`,
`RefineCompositor` obligations on `present::RefineFrameTag`).

- The default feature is std, serde/serde_json, zeroize, getrandom,
  tracing and shared crates (arcen-transport TLS only) and is enough for
  settings, saved connections, pins, the reconnect controller and the other
  pure Deck logic. `runtime` adds tokio and
  `arcen-transport/quic` (session engine, lanes, media worker); `egui` adds
  egui only (shared screens); `wgpu-presenter` adds egui-wgpu and wgpu at
  the versions eframe already resolves. CI proves the default graph pure.
- Changing a seam trait changes every Deck: describe the change in the
  trait's docs and tell the owners of `clients/macos` and `clients/windows`.
- Seams are traits only; never add an OS call or `cfg(target_os)` branch.

## Shared consumer register

Each shared crate's `consumers.json` lists every package that uses it (Cargo
normal/build/dev dependencies, compiled-in shared files, installers that
embed a consumer, and `indirect` products that reach the crate only through a
non-shared helper), the separate product paths each one serves, and the
validation IDs from `scripts/ci/validation-profiles.toml` that protect each
path. The readable view is the generated
[`docs/architecture/shared-consumers.md`](../docs/architecture/shared-consumers.md).

- **Adding a product path, or a new dependency on a shared crate:** register it
  in that crate's `consumers.json` (stable path ID, real entry points, build
  profiles, validation IDs, owner), then run
  `python3 scripts/check_shared_contracts.py --write-report`. The default check
  fails on an unregistered or stale consumer, wrong features/targets, unknown
  IDs, missing entry points, or a stale report.
- **Changing a shared crate:** run
  `python3 scripts/check_shared_contracts.py --impact --base main --commands`
  and run, or obtain revision-bound evidence for, every listed validation of
  every affected consumer path — not only this crate's own tests. `--gate`
  with `--evidence FILE` fails while a mandatory validation lacks a `passed`
  result for the head revision.
- Removing a register row or catalogue validation changes what a shared change
  must prove; it needs review by the affected path owner. A validation dropped
  from the catalogue or from a path record, or weakened in the catalogue,
  keeps `--gate` closed until the owner passes `--acknowledge-retired ID`,
  even when the change touches no shared source.

## Shared video contract

`arcen-media` owns the portable colour vocabulary, complete video
configuration, degradation taxonomy, conversion maths, and READY/plan truth.
Platform directories choose and operate native capture APIs; they must not
duplicate these decisions.

- Keep eight-bit BGRA, packed RGB10, FP16-scRGB-to-SDR, and
  FP16-scRGB-to-PQ conversions as explicit source-specific functions.
- Transfer and primaries are first-class axes. Bit depth alone never means
  HDR, and a host may retain PQ/HLG only when its native provider proves an HDR
  source.
- `PlanDegradation` must report changes to matrix, primaries, transfer, range,
  depth, chroma, codec, fps, geometry, and cursor authority rather than hiding
  them in a platform adapter.
- Shared code may describe capture capabilities and conversion contracts, but
  must not import D3D11/WGC, X11/XShm, CUDA/NVENC, VideoToolbox, or Metal.
