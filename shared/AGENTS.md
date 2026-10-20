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
and `shared/usb-bridge` (`arcen-usb-bridge`, safe Hard USB policy/state core)
are live and in workspace `members`.
`arcen-observability` is the OS-free
tracing and bounded-I/O runtime; `arcen-telemetry` remains pure.
See `docs/adr/0004-platform-scope.md` and `docs/architecture/transport.md`.

Validate with `python3 -m unittest scripts/test_validate_observability.py`,
`cargo fmt --all --check`,
`cargo test --locked -p arcen-identity -p arcen-input -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge -p arcen-cp-ipc -p arcen-keel`,
`cargo clippy --locked -p arcen-identity -p arcen-input -p arcen-media -p arcen-observability -p arcen-outputs -p arcen-protocol -p arcen-session -p arcen-telemetry -p arcen-transport -p arcen-usb-bridge -p arcen-cp-ipc -p arcen-keel -- -D warnings`,
the `arcen-media/software-h264-source` test and strict-Clippy gates on Rust
1.89+ with C++ and NASM,
`cargo test --locked -p arcen-transport --features quic`, and the CI
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
There is no single-platform `--workspace` build. `arcen-protocol`,
`arcen-input`, and `arcen-transport` retain crate-level `unsafe_code = "forbid"`;
`arcen-media` is crate-wide `unsafe_code = "deny"` with a single reviewed
OpenH264 raw `SetOption` exception in `software_h264.rs`. Shared manifests must
not reference `hosts/`, `clients/`, or `packaging/`.

Escalate public API, wire compatibility, trust-boundary, cryptography, and new
third-party dependency changes to Release/Security. Notify every affected
product owner before changing shared behavior.

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
