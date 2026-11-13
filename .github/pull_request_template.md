<!-- SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception -->
<!-- Copyright (C) 2026 TUTTIFRUTTI AS -->
## What and why

## Built and tested

CI does not build the platform crates, so this section is the evidence.
Say what you built and ran, on which OS, and what you did not.

- [ ] macOS (Deck / Pier): …
- [ ] Linux Pier: …
- [ ] Windows Pier: …
- [ ] Shared crates: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --all --check`
- [ ] `scripts/ci/check-publication-hygiene.sh`

## Checklist

- [ ] Portable logic is in `shared/`; platform code is only what the OS forces.
- [ ] New files carry the SPDX licence header (`reuse lint` passes).
- [ ] This pull request contains no private data: no credentials or keys, no
      internal host names or addresses, no customer data, no unredacted logs.
- [ ] CLA: this is not my first pull request, or I have pasted the CLA
      statement from `CLA.md` ("How to accept") below.
- [ ] How to credit me (name or handle): …
