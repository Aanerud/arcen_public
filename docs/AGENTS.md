# Documentation Ownership

**Owner roles:** Shared/Architecture and Release/Security

Shared/Architecture owns architecture and ADR accuracy. Release/Security owns
operations, security, legal cross-references, and release claims.

Validate links, commands, component names, and version references against the
workspace. Do not state unverified certification, compliance, residency, or
legal conclusions.

When documenting video, name the complete source-to-presentation pipeline.
Do not use "10-bit" as shorthand for HDR or describe Grading/HDR as labels on
the eight-bit fast path. State capture provider, source precision, conversion,
copy boundary, encoder format, negotiated transfer/primaries, Deck
presentation, and any truthful degradation.

Write user-facing text (release notes, `CHANGELOG.md` entries under
`[Unreleased]`, public GitHub release bodies and Deck UI strings) plainly: one sentence
per change from the user's side, grouped by product, with no internal causes,
file names, PR numbers or ADRs. That detail stays in commits and pull requests.

Escalate product-specific behavior to the relevant owner and all security,
distribution, entitlement, and legal wording to Release/Security.
