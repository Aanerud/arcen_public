# Supply-chain security

How Arcen's source, dependencies and release artefacts are kept trustworthy,
and which controls are automated.

## Continuous integration

CI (`.github/workflows/ci.yml`) runs only when started by hand
(`workflow_dispatch`), on GitHub-hosted Linux runners, with read-only
repository permissions. It never uses `pull_request_target`, self-hosted
runners, production environments, signing material or deployment
credentials. Every third-party action is pinned to a full commit SHA.

It runs:

- publication hygiene and REUSE licence compliance;
- the documentation content guard;
- formatting, and the tests and strict Clippy of the shared crates;
- the dependency-purity proofs: product graphs are QUIC and TLS 1.3 only,
  and the default graphs of `arcen-transport`, `arcen-media`,
  `arcen-outputs` and `arcen-deck-core` stay free of optional networking,
  native codecs and UI;
- verification of the governed vendored sources;
- a Gitleaks scan of the checked-out history.

CI does not build the platform crates; each runs on its own operating
system, and a pull request states what was built there. `deny.toml` holds the
dependency policy (licences, registries, Git sources), but Cargo Deny and
GitHub dependency review are not CI jobs, so neither is claimed as an
automated gate.

## Dependency and action updates

A dependency change needs:

1. a reviewed `Cargo.lock` diff;
2. formatting, the shared-crate tests and strict Clippy, and the affected
   product builds and tests on their target operating system;
3. a review of new licences, registries, Git sources, build scripts and
   native code; and
4. Release/Security review, because the dependency, licence and SBOM checks
   are not automated.

An action update needs a review of the upstream release and source diff, the
release tag resolved to its commit, and the full 40-character commit recorded
in the workflow. Mutable action tags are refused.

## TLS dependencies and packaging

Direct QUIC uses `rustls` with the `ring` provider, `x509-parser` for bounded
X.509 inspection, and `subtle` for constant-time pin comparison, each pinned
in `Cargo.lock`; their lockfile entries and notices need Release/Security
review. The default `arcen-transport` graph is library-only and carries no
Quinn or Tokio; product crates enable QUIC explicitly. CI tests that feature
and proves that no product graph carries WebSocket networking.

OpenSSL is a system packaging prerequisite of the Linux SMB certificate
helper only. The Pier does not link it, never invokes it, and never uses it as
a certificate source at run time. Distribution packaging records the actual
system or bundled version and its notices; it is not a Rust dependency.

Packaging and run time are separate privilege boundaries. The
generate-if-missing helpers refuse partial or custom pairs and write explicit
SAN-bearing server-auth certificates with protected permissions. The Pier
checks the SAN, key class, strength and match, validity, purpose, the opened
files and the platform's filesystem or DACL policy itself before it listens or
reloads. A helper's success alone never admits a certificate.

## Native media source builds

`arcen-media/software-h264-source` is optional and off by default. It pins
crates.io `openh264` and `openh264-sys2` to 0.9.7 and enables only the
latter's bundled `source` path; the bundled Cisco tree is commit
`a8e04adb69c79757da014007d4694684a64c7b74`. `libloading`, a runtime download
and Cisco's precompiled binary are not admitted.

The feature adds unsafe C/C++ codec code and native build tooling, so it
needs reviewed lockfile checksums, a compiler and NASM preflight,
target-specific tests and package inspection. The default-media check proves
the codec and its native build graph absent.

- **Windows** also needs MSVC, Rust `+crt-static`, C/C++ `/MT`, inspection of
  the archive members, and refuses dynamic MSVC, GNU or OpenH264 runtime
  dependencies.
- **Linux and macOS** packages refuse a shared OpenH264 dependency and a
  nested codec payload.

The wrapper crates declare Rust 1.85, but their `wide` and `safe_arch`
dependencies declare 1.89, so the feature needs Rust 1.89; the default
workspace keeps its 1.85 floor. No fork is approved to lower it.

BSD-2-Clause notice compliance is separate from H.264 patent and distribution
questions. Arcen does not claim that the terms or royalty arrangements of
Cisco's separately distributed binary cover Arcen's source-built output.
External distribution of the feature needs Release/Security and legal
approval, a target-specific SBOM and notices, and physical acceptance. See
[`../architecture/media-plan-resolution.md`](../architecture/media-plan-resolution.md).

## SBOM and provenance

CI emits no SBOM or provenance. `scripts/ci/generate-supply-chain.sh` writes
a CycloneDX SBOM with `cargo cyclonedx` when run by hand. Release automation,
when it exists, must emit:

- an SBOM in a reviewed format, naming the tool and its version;
- an in-toto statement with the SLSA provenance v1 predicate;
- the repository, source commit, workflow identity, run ID and lockfile
  digest; and
- `SHA256SUMS` over every generated metadata file.

Until then, `legal/ORIGINS.md` records the exact OpenH264 feature subset and
its lockfile checksums. That record does not replace a complete
target-specific release SBOM.

An unsigned source-provenance statement gives structured build metadata, but
it is neither an attestation nor proof of runner integrity. GitHub artifact
attestations are not enabled and not claimed.

## Release artifact boundary

Releases are built, signed and notarised by hand; no staging or production
workflow exists. See [Versioning and releases](../operations/versioning-and-releases.md).

Release automation must meet this contract:

- Staging builds artefacts once, addressed by version and commit.
- Production accepts only a successful staging run from `main` whose commit
  matches the reviewed tag, downloads those exact artefacts, and never
  rebuilds them.
- Platform signing hooks and the detached release-manifest signing hook are
  mandatory and fail closed when a hook or its output is missing.
- Signing keys and notarisation credentials live only in reviewer-protected,
  least-privilege environments and are never exposed to pull-request code.
