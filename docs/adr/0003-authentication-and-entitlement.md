# ADR 0003: Authentication and Entitlement Separation

**Status:** Superseded for current direct-Pier entitlement by
[ADR 0006](0006-offline-pier-licensing.md). Retained for the dormant future
OIDC/authoritative-session and enterprise floating model.

## Decision

Keep user identity, OS machine authentication, and product entitlement as three
separate planes. Use generic OIDC with Microsoft Entra as the first provider
profile. Access customer-hosted Reprise License Manager only through the
vendor-neutral concurrent-seat lease interface.

The authoritative active-session order is OIDC identity, entitlement holder,
OS machine authentication, grant validation and atomic replay consumption,
authorized capability/transport negotiation, then streaming. A short-lived
Arcen session grant has an injected signature-verifier boundary; shared code
does not select keys, algorithms, or network metadata discovery. Entra is
represented only through ordinary issuer, audience, subject, and tenant
configuration.

Session-grant schema version 1 requires explicit issuer, audience, subject,
tenant, authenticated client identity, target Host identity, active-session
identity, nonce, issued-at, and expiry dimensions. Validation requires local
expectations for every identity/binding dimension. Unversioned grants are not
treated as version 1 and unsupported versions fail closed. Non-cloneable,
atomically consumed evidence, not raw or merely validated claims, flows into
each connection admission.

Grant nonces are consumed through a durable atomic insert-if-absent contract.
All Gateway instances in a deployment share one authoritative replay store. A
successful nonce remains protected until its grant expires; shared code defines
the atomic and retention semantics without selecting a database.

One concurrent seat aggregates by tenant, feature, and human. Each overlapping
active session has an idempotent holder attached to that shared lease.
Checkout, holder attachment, holder heartbeat, idempotent holder detachment,
stale-holder reconciliation, final provider release, and crash recovery are
explicit provider-neutral operations. One holder detaching never releases a
seat while another holder remains.

## Consequences

No licensing backend may become an identity provider. No OIDC token alone
proves host machine identity. Authorization policy combines verified inputs
without collapsing their lifecycles or audit records. Gateway deployments need
durable shared replay storage; product cleanup must detach its own seat holder,
not release a lease globally.
