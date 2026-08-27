# ADR 0001: Workspace Boundaries

**Status:** Accepted

## Decision

Use one Rust workspace with shared libraries and independent host, client, and
gateway products. Shared crates must not depend on product crates. Product
crates may depend on shared crates but never on another product.

## Consequences

Shared interfaces require cross-component review. Product-specific OS and
deployment code stays at the edge.

Workspace `members` are scoped to the currently active products:
`arcen-identity`, `arcen-input`, `arcen-media`, `arcen-observability`,
`arcen-protocol`, `arcen-session`'s dependency-light default, `arcen-keel`,
`arcen-telemetry`, `arcen-transport`'s dependency-light TLS default, the macOS
Deck, the Linux and Windows Piers, capture/helper crates, and the Windows
Credential Provider. Deferred products, optional feature graphs, and dormant
shared crates remain on disk but out of
the explicit `members` list and default build until their milestone (see ADR
0004). Because each product targets a different operating
system, there is no single-platform `--workspace` build. CI runs the pure shared
gates centrally, builds each active product on its target OS, and runs the
target-specific tests currently configured there.
`unsafe_code` is `warn` workspace-wide for the migrated FFI-heavy products;
the pure live shared crates reassert `forbid`.
