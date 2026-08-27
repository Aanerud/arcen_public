# Packaging Ownership

**Owner role:** Release/Security

Own installer, package, container, signing, provenance, artifact retention, and
release metadata under `packaging/`. Platform owners co-own their corresponding
subdirectory.

Validate the affected product crate on its target OS plus packaging-specific
tests and generated notices. Never embed credentials or signing material.

Escalate package format or platform behavior changes to the matching product
owner; escalate every signing, entitlement, third-party notice, or distribution
change to Release/Security.
