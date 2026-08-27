# Supply-Chain Security

> **STATUS (2026-07-14): CURRENT CI BOUNDARY; DEPENDENCY, SBOM, AND RELEASE GATES
> DEFERRED.** `.github/workflows/ci.yml` currently provides per-platform builds,
> pure-crate tests/strict Clippy, formatting, and Gitleaks. Cargo Deny,
> dependency review, SBOM/source-provenance generation, artifact attestation,
> and release workflows must be rebuilt after the dependency set and release
> process are curated. The deferred controls below are requirements, not claims
> about current automation.

## Hosted pull-request boundary

Pull-request code runs only on GitHub-hosted runners with read-only repository
permissions. CI does not use `pull_request_target`, self-hosted runners,
production environments, signing material, or deployment
credentials. Third-party actions are pinned to full commit SHAs.

Gitleaks scans the checked-out history. Cargo Deny and GitHub dependency review
are not current CI jobs; neither may be claimed as an automated gate.

## Dependency and action updates

Dependency changes require:

1. a reviewed `Cargo.lock` diff;
2. formatting, the pure-crate test/strict-Clippy gates, and affected product
   builds/tests on their target OS;
3. review of new licenses, registries, Git sources, build scripts, and native
   code; and
4. an explicit Release/Security review while automated Cargo Deny, dependency
   review, and SBOM gates remain deferred.

Action updates require reviewing the upstream release and source diff, resolving
the release tag to its underlying commit, and recording the full 40-character
commit in the workflow. Mutable action tags are not accepted.

## TLS lifecycle dependencies and packaging

The active direct-QUIC lifecycle uses pinned production dependencies
`rustls` 0.23.41 with ring 0.17.14, `x509-parser` 0.18.1 for bounded X.509
inspection, and `subtle` 2.6.1 for constant-time typed pin comparison. Their
lockfile entries and release notices require Release/Security review.
`arcen-transport`'s library-only default dependency graph remains free of the
optional Quinn/Tokio QUIC graph; product crates explicitly enable QUIC. CI
tests the opt-in feature and proves product graphs do not enable dormant
`wss-compat`.

OpenSSL is a system packaging prerequisite for the Linux SMB certificate
helper only. It is not linked into Pier, is not invoked by the service, and is
not a runtime certificate source. Distribution packaging must inventory the
actual system/bundled version and notices rather than treating the helper
prerequisite as a Rust dependency.

## Native media source builds

`arcen-media/software-h264-source` is optional and non-default. It pins
crates.io `openh264` and `openh264-sys2` to 0.9.7 and enables only the latter's
bundled `source` path. The crate-supplied Cisco tree identifies commit
`a8e04adb69c79757da014007d4694684a64c7b74`. `libloading`, runtime download, and
Cisco's precompiled binary are not admitted.

The enabled graph adds unsafe C/C++ codec code plus Rust and native build
tooling. It therefore requires reviewed lockfile checksums, compiler/NASM
preflight, target-specific tests, and package inspection. The default-media
gate proves the codec and native build graph absent. Windows additionally
requires MSVC, Rust `+crt-static`, C/C++ `/MT`, archive-member inspection, and
rejection of dynamic MSVC/GNU/OpenH264 runtime dependencies. Linux and macOS
packages reject a shared OpenH264 dependency and nested codec payload.

The wrapper crates declare Rust 1.85, but their exact `wide` 1.1.1 and
`safe_arch` 1.1.0 dependencies declare 1.89. The source feature therefore has a
Rust 1.89 floor; the dependency-light default workspace retains its 1.85 floor.
No fork is approved to lower that requirement.

BSD-2-Clause notice compliance is separate from H.264 patent/distribution
analysis. Arcen does not claim that Cisco's separately distributed precompiled
binary terms or royalty arrangements cover Arcen's source-built output.
External distribution remains subject to Release/Security/legal approval,
target-specific SBOM and notices, and physical acceptance. See
[`../architecture/media-plan-resolution.md`](../architecture/media-plan-resolution.md).

Packaging and runtime remain separate privilege boundaries. Generate-if-missing
helpers refuse partial/custom pairs and publish explicit SAN-bearing,
server-auth certificates with protected permissions. Pier independently
revalidates SAN, key class/strength and match, validity, purpose, bounded
opened-file snapshots, and platform filesystem/DACL policy before listen or
reload. Helper success alone is not service admission.

## SBOM and provenance

Current CI does not install `cargo-cyclonedx` or emit SBOM/provenance metadata.
Future release automation must emit:

- a reviewed SBOM format and tool/version;
- an in-toto statement using the SLSA provenance v1 predicate;
- the repository, source commit, workflow identity, run ID, and lockfile digest;
  and
- `SHA256SUMS` over every generated metadata file.

Until that automation exists, `legal/ORIGINS.md` records the exact
OpenH264 feature subset and lockfile checksums. That reviewed subset is not a
substitute for the complete target-specific release SBOM.

An unsigned source-provenance statement would provide structured build metadata
but would not be an attestation or proof of runner integrity. GitHub artifact
attestations must not be enabled or claimed until the repository account and
reviewed release design support them.

## Release artifact boundary

No staging or production workflow is currently present. Future staging must
build version-and-commit-addressed artifacts once. Production must accept only
a successful staging run from `main` whose commit matches the reviewed tag,
download those exact artifacts, and never rebuild them.

Future platform signing hooks and the detached release-manifest signing hook
are mandatory and must fail closed on missing hooks or outputs. Signing keys,
and notarization credentials belong only in
reviewer-protected, least-privilege environments and must never be exposed to
pull-request code.
See [Release automation](../operations/release-automation.md) for the current
account limitations and enablement checklist.
