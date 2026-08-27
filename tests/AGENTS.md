# Cross-Component Test Ownership

**Owner roles:** Shared/Architecture and Release/Security

Own conformance, compatibility, end-to-end, hardware, network, and recovery
test contracts. Product owners supply platform fixtures without coupling
product crates to one another.

**Current state (2026-07-14):** automated coverage lives in the active product
crates. `tests/{compatibility,conformance,network}` are dormant packages out of
workspace `members`; the hardware workflow is deferred and not present in
`.github/workflows/`.

Run the root shared-crate test and strict Clippy gates plus affected product
tests on their target OS. Do not claim a workspace-wide suite. Hardware tests
remain disabled until protected self-hosted runners, environments, and tracked
harnesses are available.

Escalate protocol expectation changes to Shared/Architecture and trust,
credential, hardware-runner, or release-gate changes to Release/Security.
