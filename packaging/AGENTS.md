# Packaging Ownership

**Owner role:** Release/Security

Own installer, package, container, signing, provenance, artifact retention, and
release metadata under `packaging/`. Platform owners co-own their corresponding
subdirectory.

Validate the affected product crate on its target OS plus packaging-specific
tests and generated notices. Never embed credentials or signing material.

The Linux and Windows installers each contain all supported capture pipelines;
HDR/Grading are not optional payloads or separate installers. Rebuild the Pier
before rebuilding its embedding installer. Build all release artifacts from
the same source identity, regenerate `dist/SHA256SUMS.txt`, and require the Deck
zip to contain a Developer ID signed, notarized, stapled, Gatekeeper-clean app.

Escalate package format or platform behavior changes to the matching product
owner; escalate every signing, entitlement, third-party notice, or distribution
change to Release/Security.
