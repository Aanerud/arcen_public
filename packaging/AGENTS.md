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
the same source identity, assemble them in `dist/releases/<version>/` with its `SHA256SUMS.txt`, and require the Deck
zip to contain a Developer ID signed, notarized, stapled, Gatekeeper-clean app.

Clean up always. An uninstaller removes everything any Arcen build installed
(jobs, services, helpers, drivers, sockets, support directories, firewall
rules) and verifies nothing is left. An upgrade removes what earlier builds
installed and the new build no longer uses; keep retired labels and paths in
the legacy lists so this never depends on memory. Each host has one bounded
helper: do not add another privileged process or launchd job without
Release/Security approval. Lab and test runs leave the machine as they found
it, configuration byte-identical.

Escalate package format or platform behavior changes to the matching product
owner; escalate every signing, entitlement, third-party notice, or distribution
change to Release/Security.
