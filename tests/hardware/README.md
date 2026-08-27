# Hardware Acceptance

> **STATUS (2026-07-14): DEFERRED.** No hardware-acceptance workflow is present
> in `.github/workflows/`. Do not register self-hosted runners or enable this
> gate until protected environments and runner isolation are enforceable.

Hardware acceptance suites cover Linux GPU, Windows GPU, and macOS client
hardware. A future protected workflow must define explicit runner labels and
environment-scoped harness configuration.

These suites must never execute code from an untrusted pull request.

A future protected workflow must pass explicit scenario arguments to a tracked,
repository-relative harness. Until a platform owner supplies that harness and
the required protected environment, hardware scenarios remain disabled.
