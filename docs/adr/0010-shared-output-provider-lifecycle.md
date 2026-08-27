# ADR 0010: Shared Output Provider Lifecycle

**Status:** Accepted (2026-08-10). Design gate only. At this commit there is no
`shared/outputs` directory, no `arcen-outputs` package, no workspace member, and
no host migration. This ADR freezes the shape that the crate and the migration
must implement.

## Context

Three different host-local output contracts exist at this commit, and they were
written independently:

**1. The Linux transaction driver.** `hosts/linux/src/session/output_provider.rs`
defines a `pub(crate) trait OutputProvider` with `type Prepared` and
`type Error: Debug`, a synchronous `dry_run(&self)`, `async bind`, `async
verify`, a synchronous `commit(&mut self, Prepared)`, and `async rollback`. The
driver is the free function `provision_output(provider)`, which consumes the
provider and returns it on success. Errors are typed:
`OutputStage { DryRun, Bind, Verify, Commit }` and
`OutputProvisionError<E> { Operation { stage, source }, Rollback { stage,
source, rollback } }`. Capabilities are
`{ min_regions, max_regions, physical, headless }` with one constant,
`DEDICATED_XORG`. The single implementer is `DedicatedXorgProvider` in
`hosts/linux/src/session/launcher.rs`. It stores the bound resource in the
provider (`output: Option<DedicatedXorg>`) plus a `committed: bool` flag, checks
its own region count inside `dry_run`, and recovers the resource afterwards
through `into_committed()`.

**2. The Windows transaction driver.** `hosts/windows/src/output_provider.rs`
defines a `pub(crate) trait OutputProvider` with `type Prepared` and
`type Binding`, and a struct driver `OutputProviderTransaction<P>` that owns
both the provider and the binding. `acquire()` runs a capability gate, then
`dry_run`, `bind` (which consumes `Prepared` and produces `Binding`), then
`verify`, and rolls back if verification fails. `commit`, `rollback`,
`is_armed`, `report`, and `applied_plan` are methods on the transaction after
acquisition. Everything is synchronous. Every error is a `String`, and a
rollback failure is flattened into a formatted message
(`"{error}; output-provider rollback also failed: {rollback_error}"`).
Capabilities are
`{ min_regions, max_regions, exact_modes, signed_coordinates,
persistent_dedicated_desktop, recovery_rollback }`, and the gate returns
formatted strings. The implementers are `PhysicalOutputProvider` and
`IddCxOutputProvider` in `hosts/windows/src/display.rs`. `MultiDisplayLease`
wraps the transaction in an enum, and its `Drop` restores when `is_armed()` is
true, using `tokio::task::block_in_place` on a multi-thread runtime.

**3. The Wayland capability contract.** `hosts/linux/src/display/wayland.rs`
defines a third, unrelated `WaylandOutputSource` behind the default-off
`wayland-provider` feature. It has `type Error: Error + Send + Sync + 'static`,
`capabilities()`, and `snapshot()`. Its `WaylandOutputCapabilities` is
`{ enumerate_outputs, xdg_output_logical_regions, fractional_scale,
mutter_virtual_output }`, which are compositor detection facts, not lifecycle
capabilities. `detect_output_capability` is fail-closed and returns typed
unavailable reasons. This is an inventory and selection seam. It performs no
transaction and has no implementer. See
[`../architecture/linux-wayland-provider.md`](../architecture/linux-wayland-provider.md).

So the same product concern has three names for `OutputProvider`, two
incompatible drivers, two incompatible capability vocabularies, one typed error
model and one string error model, one async model and one sync model, and two
different answers to where bound state lives and where the capability gate runs.
The consolidation plan calls for one shared crate, `arcen-outputs`, and the
migration cannot start until the shape stops moving.

[ADR 0009](0009-multi-monitor-foundation.md) already froze the product
invariants this lifecycle has to carry: two-phase admission, atomic topology,
and the non-headless rollback invariant. [ADR 0008](0008-virtual-display-for-windows-hosts.md)
already committed to a second Windows implementation of the same lifecycle
(IddCx) alongside the physical one. Neither ADR says what the code shape is.
This ADR does, and changes neither of them.

## Decision

### 1. Crate and dependency boundary

Create `shared/outputs`, package `arcen-outputs`, owned by Shared/Architecture.

- It declares exactly two dependencies: `arcen-media` (for
  `MAX_MULTI_MONITOR_COUNT`, currently `4` in `shared/media/src/multi_monitor.rs`,
  and for the shared region value objects the later modules need) and
  `arcen-telemetry` (for `CorrelationId`). It declares no other dependency, in
  `[dependencies]`, `[dev-dependencies]`, or `[build-dependencies]`.
- It contains no executor, no timer, no thread spawn, no task spawn, and no I/O
  of its own. `tokio`, `tokio-util`, `quinn`, `bytes`, `futures`, and
  `async-trait` are all excluded, and so is every `hosts/`, `clients/`,
  `gateway/`, and `packaging/` crate.
- Its own tests drive futures with a small in-crate `block_on` built on
  `std::thread::park` and a hand-written `RawWaker`. No `#[tokio::test]`, because
  a dev-dependency would show up in `cargo tree --locked -p arcen-outputs -e all`
  and defeat the purity proof that the `arcen-outputs-purity-gate` work item
  adds.
- Providers are used through generics. Object safety is not a requirement, and
  no `dyn OutputProvider` exists. Hosts that need to pick between backends use
  an enum, as `hosts/windows/src/display.rs` already does with
  `MultiDisplayTransaction`.

### 2. The frozen trait

```rust
pub trait OutputProvider {
    /// Host-owned request. The shared crate never inspects it.
    type Plan;
    /// Inert result of preflight. Holds no OS resource.
    type Prepared;
    /// Owns every OS-visible resource created by `bind`.
    type Binding;
    /// Host-shaped result, meaningful only after `verify` returned `Ok`.
    type Evidence;
    type Error: core::fmt::Debug + core::fmt::Display + Send + 'static;

    fn capabilities(&self) -> OutputCapabilities;
    fn demand(&self, plan: &Self::Plan) -> OutputDemand;

    fn preflight(
        &mut self,
        plan: &Self::Plan,
        context: &OutputContext,
    ) -> Result<Self::Prepared, Self::Error>;

    fn bind(
        &mut self,
        prepared: Self::Prepared,
    ) -> impl Future<Output = Result<Self::Binding, BindFailure<Self::Error>>> + Send;

    fn verify(
        &mut self,
        binding: &mut Self::Binding,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn commit(
        &mut self,
        binding: &mut Self::Binding,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn rollback(
        &mut self,
        binding: &mut Self::Binding,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence;
    fn is_armed(&self, binding: &Self::Binding) -> bool;
}
```

Why each part is fixed:

- **Future-returning, not `async fn`.** `async fn` in a trait desugars without a
  `Send` bound, so a host could not `tokio::spawn` the transaction. Writing
  `-> impl Future<Output = ...> + Send` puts the bound in the frozen shape.
  This uses only `core::future::Future`, so it needs no dependency. Async
  implementers may still write `async fn` in the `impl` block; the compiler
  checks the `Send` bound for them. Synchronous implementers return
  `core::future::ready(...)`, which is how both Windows providers will satisfy
  the async shape while keeping their blocking CCD, NVAPI, and IddCx calls
  unchanged.
- **`preflight` is synchronous.** Both current `dry_run` implementations are
  synchronous, and preflight must not mutate anything OS-visible, so it must not
  need an executor. A provider that needs asynchronous probing does it in `bind`,
  which already owns the failure and cleanup path.
- **`Plan` is an associated type.** `WindowsTopologyPlan`,
  `LinuxTopologyPlan`, and any future Wayland plan stay in their host crates.
  The shared crate never reads a plan. That keeps the dependency direction one
  way and prevents host topology types from leaking into `shared/`.
- **`demand` is the bridge to the shared capability gate.** The provider
  translates its host plan into a small semantic `OutputDemand`, which is the
  only thing the shared admission check reads.
- **`Error` is not `std::error::Error`.** It is `Debug + Display + Send +
  'static`. `String`, which Windows uses today, satisfies that, so the migration
  can move without a simultaneous error-type rewrite. `Sync` is deliberately not
  required, because the driver only moves errors, never shares them.
- **No GATs.** `Prepared`, `Binding`, and `Evidence` carry no lifetime
  parameter. A provider that borrows host state parameterises the provider type
  instead, so `OutputTransaction<DedicatedXorgProvider<'a>>` is valid and
  `hosts/linux/src/session/launcher.rs` keeps its borrowed paths.
- **`evidence` borrows from both.** The `&'a self, &'a Self::Binding ->
  &'a Self::Evidence` signature is the generalisation of the existing Windows
  `report` and `applied_plan` accessors, and it lets a provider store evidence
  in either half.

Ownership rules, stated as falsifiable claims the implementation must enforce:

- `preflight` makes no OS-visible mutation. Dropping a `Prepared` is always
  safe and never requires rollback. A `Prepared` that owns a handle, a journal
  arm, or a child process is a defect.
- `Binding` owns every OS-visible resource, handle, journal arm, and child
  process created by `bind`. `rollback` needs nothing else.
- The provider holds only driver-level state that outlives one attempt, for
  example a loaded NVAPI driver or an inherited IddCx control handle. Per-attempt
  state belongs in `Binding`.
- `Evidence` is only read through the transaction, which only exists after
  `verify` succeeded. A provider may keep a placeholder before that, as
  `empty_multi_display_report` does today, but nothing outside the provider can
  observe it.

### 3. The frozen driver

```rust
#[must_use = "an acquired output transaction is armed until it is committed or rolled back"]
pub struct OutputTransaction<P: OutputProvider> { /* provider, binding, state */ }

pub enum OutputTransactionState { Bound, Committed, RolledBack }

impl<P: OutputProvider> OutputTransaction<P> {
    pub async fn acquire(provider: P, plan: &P::Plan, context: &OutputContext)
        -> Result<Self, OutputTransactionError<P::Error>>;
    pub fn capabilities(&self) -> OutputCapabilities;
    pub fn evidence(&self) -> &P::Evidence;
    pub fn is_armed(&self) -> bool;
    pub const fn state(&self) -> OutputTransactionState;
    pub async fn commit(&mut self) -> Result<(), OutputTransactionError<P::Error>>;
    pub async fn rollback(&mut self) -> Result<(), P::Error>;
    pub fn into_committed(self) -> Result<CommittedOutput<P>, Self>;
}

pub struct CommittedOutput<P: OutputProvider> { /* provider, binding */ }
```

- `acquire` runs admission, then `preflight`, then `bind`, then `verify`. A
  returned transaction has passed verification and has not been committed. This
  is the Windows shape, generalised; the Linux free function `provision_output`
  is replaced by it.
- `commit` failure rolls back, and the transaction ends in `RolledBack`. That
  keeps the current Linux behaviour, where `provision_output` rolls back a failed
  commit, and it tightens Windows, where a failed `MultiDisplayLease::commit`
  currently leaves the lease armed for `Drop` to restore. The end state is the
  same; the shared driver reaches it eagerly and reports it in one typed value.
- `into_committed` succeeds only in the `Committed` state and otherwise hands
  the transaction back, still armed, so the caller must resolve it. This is the
  generalisation of the Linux `into_committed()` and `committed: bool` pair, and
  it moves that flag out of provider code.
- `rollback` is idempotent, matching the existing `if !self.recovery_armed {
  return Ok(()); }` guards in `hosts/windows/src/display.rs`.
- `OutputContext` carries `session_log_id: CorrelationId` today and grows
  additively. Windows already threads that value into `dry_run`; Linux gains it.

### 4. Typed errors

```rust
#[non_exhaustive]
pub enum OutputStage { Admission, Preflight, Bind, Verify, Commit }

pub enum OutputTransactionError<E> {
    Admission(CapabilityMismatch),
    Operation { stage: OutputStage, source: E },
    Rollback { stage: OutputStage, source: E, rollback: E },
}

pub struct BindFailure<E> { pub source: E, pub rollback: Option<E> }

#[non_exhaustive]
pub enum CapabilityMismatch {
    RegionCount { requested: usize, min: usize, max: usize },
    ExactModesUnsupported,
    SignedCoordinatesUnsupported,
    PersistentDesktopUnsupported,
    HeadlessUnsupported,
    RotationUnsupported,
    FractionalScaleUnsupported,
    RollbackGuaranteeInsufficient { required: RollbackGuarantee, provided: RollbackGuarantee },
}
```

Rules:

- A rollback failure is never flattened into a formatted string. Both the
  primary failure and the rollback failure survive as separate typed values.
  `Display` may render both, but the variant keeps both. This deletes the
  Windows `format!("{error}; output-provider rollback also failed: ...")` and
  the equivalent messages inside `hosts/windows/src/display.rs`.
- `Admission` and `Preflight` can never carry a rollback, because nothing is
  bound yet. The driver enforces this by construction, and a test asserts it.
- `bind` reports its own internal rollback through `BindFailure`. This is
  forced by the type: `bind` produces the `Binding`, so on failure there is no
  `Binding` for the driver to roll back. A provider that already mutated
  something must undo it before returning `Err`, and must report the outcome of
  that undo in `BindFailure::rollback`. The driver maps `Some(rollback)` to
  `OutputTransactionError::Rollback { stage: Bind, .. }`. Windows already rolls
  back internally in `bind` and loses the detail in a string; Linux currently
  calls the provider's `rollback` after a failed `bind`, which is a no-op there
  because it never set its output. Both converge on the typed form.
- `OutputStage` and `CapabilityMismatch` are `#[non_exhaustive]` so new stages
  and new mismatches are additive. `OutputTransactionError` is exhaustive,
  because those three outcomes are the frozen shape.

### 5. Unified capability semantics

```rust
pub struct OutputCapabilities {
    min_regions: usize,          // validated, >= 1
    max_regions: usize,          // validated, <= arcen_media::MAX_MULTI_MONITOR_COUNT
    pub surface: OutputSurface,
    pub exact_modes: bool,
    pub signed_desktop_coordinates: bool,
    pub persistent_dedicated_desktop: bool,
    pub headless_capable: bool,
    pub per_region_rotation: bool,
    pub fractional_scale: bool,
    pub rollback: RollbackGuarantee,
}

pub enum OutputSurface { SharedPhysical, DedicatedPhysical, Virtual }
pub enum RollbackGuarantee { None, BestEffort, SafePrimary, ExactRestore }

pub struct OutputDemand {
    pub regions: usize,
    pub negative_coordinates: bool,
    pub exact_modes: bool,
    pub persistent_desktop: bool,
    pub headless: bool,
    pub rotation: bool,
    pub fractional_scale: bool,
}
```

Every field is semantic, meaning it states what the provider can promise about
the resulting desktop, not how it does it:

| Capability | Meaning |
| --- | --- |
| `min_regions`/`max_regions` | Region counts the provider can serve atomically. Built through a checked constructor, so an out-of-range or inverted range cannot exist. |
| `surface` | `SharedPhysical` mutates outputs the console session also uses. `DedicatedPhysical` owns a head or display server dedicated to the remote session. `Virtual` creates monitors that did not previously exist. |
| `exact_modes` | The applied mode equals the requested mode. No nearest-match substitution. |
| `signed_desktop_coordinates` | Negative desktop origins are supported. |
| `persistent_dedicated_desktop` | The topology survives for the whole session rather than for one call. |
| `headless_capable` | The provider can serve with no monitor physically attached, for example through a synthesised EDID. |
| `per_region_rotation` | Per-region rotation is applied and verified, not ignored. |
| `fractional_scale` | A per-region scale other than a whole multiple of 120 in the shared `Scale120` domain is honoured. |
| `rollback` | The strongest guarantee the provider can prove. See section 6. |

Admission is the shared function `admits(capabilities, demand) ->
Result<(), CapabilityMismatch>`, run by `acquire` before `preflight`. It
replaces both the Windows `validate_capabilities` string gate and the ad-hoc
`supports_region_count` check inside the Linux provider's `dry_run`. The
provider supplies both sides, because only it can read its own host plan, but it
does not decide the outcome: the comparison rules, the ordering of checks, and
the resulting `CapabilityMismatch` are the shared crate's, so two hosts cannot
drift into refusing the same topology for differently worded reasons.

What must never enter `OutputCapabilities`: `mutter_virtual_output`,
`xdg_output_logical_regions`, `enumerate_outputs`, `retarget_capable`, backend
and restore-backend name strings, device names, adapter LUIDs, target ids,
journal paths, and IddCx generation numbers. Those are host facts. They stay in
the host detection reports and in `Evidence`, which is an associated type
precisely so the shared crate never has to name them.

### 6. Arming, rollback, and the non-headless invariant

`is_armed(binding)` means exactly this: the provider holds an outstanding
teardown obligation whose omission would leave the host in a state the operator
did not choose. It becomes true at the first thing `bind` creates that outlives
a crash, whether that is an OS-visible mutation or a recovery artifact armed
before the mutation, such as the journal and watchdog
`PhysicalOutputProvider::bind` writes in `hosts/windows/src/display.rs`. It
stays true until `rollback` completes, or until `commit` explicitly releases the
obligation.

- **`commit` is not required to disarm.** `PhysicalOutputBinding::commit` in
  `hosts/windows/src/display.rs` removes the recovery journal and sets
  `recovery_armed = false`, because the applied physical topology is now the
  session's desktop. `IddCxOutputProvider::commit` verifies swapchains and
  leaves the binding armed, because the virtual monitors must still be removed
  at session end. Both are correct. The shared driver never asserts that
  `is_armed()` is false after commit, and the state machine tracks
  `OutputTransactionState` separately from arming.
- **Non-headless invariant, restated at the code boundary.** Returning `Ok(())`
  from `rollback` is a claim. For `SharedPhysical`, it claims the host has at
  least one active, usable output, either the exact pre-bind topology or a
  verified safe-primary topology. For `DedicatedPhysical` and `Virtual`, it
  claims every resource the provider created has been released and the console
  topology was not disturbed. A provider that cannot prove its claim returns
  `Err` rather than `Ok`. This is the ADR 0009 invariant expressed as a
  postcondition on one function.
- `admits` rejects any `SharedPhysical` provider declaring a `rollback` weaker
  than `SafePrimary`. `ExactRestore` additionally claims the pre-bind topology
  is restored exactly.
- `rollback` is idempotent and safe to call repeatedly.

### 7. Cancellation, drop, and the last-resort path

- Dropping a transition future is allowed, and it is not an undo. A provider
  must record OS-visible mutation into `Binding` or into an out-of-process
  journal before the first await point that can be cancelled, so a later
  `rollback` still undoes it.
- `bind` is the one stage where cancellation cannot be repaired by the driver,
  because no `Binding` exists yet. Every provider must therefore own a
  synchronous last-resort release that does not require being polled again:
  either a `Drop` implementation on the resource it holds, or an out-of-process
  recovery guarantee. Windows physical uses the recovery journal plus the
  watchdog armed in `bind`; IddCx uses the control handle whose closure removes
  the whole virtual topology; dedicated Xorg owns the child process. Each
  provider states which one it relies on, and its `RollbackGuarantee` must be
  consistent with it.
- **`OutputTransaction` does not roll back in `Drop`.** It cannot await, and the
  crate embeds no executor, so a `Drop` rollback would either block a runtime
  worker or silently skip. It is `#[must_use]` instead, and the host keeps its
  own armed-drop guard. `MultiDisplayLease::drop` in
  `hosts/windows/src/display.rs` stays where it is, keeps its `is_armed()` check,
  and keeps its `tokio::task::block_in_place` decision, because that policy needs
  the host's runtime knowledge and must not move into a dependency-free shared
  crate.
- The shared crate applies no timeout and no retry. Every deadline stays with
  the provider, including the Windows mode-settle wait, the ten-second IddCx
  enumeration and swapchain waits, and the Linux `wait_ready` wait.

### 8. Send, Sync, and lifetimes

- Every transition future is `Send`, so a host may drive the transaction inside
  a spawned task. The `acquire` and `commit` futures are `Send` when the
  provider, its binding, and its plan are.
- The provider is not required to be `Sync`, and neither is `Binding`.
- `Error` is `Send + 'static` and is not required to be `Sync`.
- No associated type carries a lifetime; providers carry them instead.
- `'static` is not required of the provider.

This shape is not hypothetical. It was compiled as a standalone probe on edition
2024 with two implementers, one fully synchronous provider returning
`core::future::ready` and one borrowing `async fn` provider, with a `Send`
assertion on the `acquire` future for both. The only language feature it relies
on beyond the workspace baseline is return-position `impl Trait` in traits,
stable since Rust 1.75 and therefore below the workspace
`rust-version = "1.85"`.

## Migration mapping

The migration is the separate `migrate-output-providers` work item. This ADR
assigns it. It changes no code.

### Linux transaction driver

| Existing item in `hosts/linux/src/session/output_provider.rs` | Destination | Treatment |
| --- | --- | --- |
| `trait OutputProvider` | `arcen_outputs::OutputProvider` | Deleted from the host, replaced by the shared trait. |
| `fn provision_output` | `OutputTransaction::acquire` | Deleted. Call sites in `hosts/linux/src/session/launcher.rs` use the transaction. |
| `enum OutputStage` | `arcen_outputs::OutputStage` | Deleted, superset adopted (`Admission` and `Preflight` added, `DryRun` renamed to `Preflight`). |
| `enum OutputProvisionError<E>` | `arcen_outputs::OutputTransactionError<E>` | Deleted, gains the `Admission` variant. |
| `struct OutputProviderCapabilities` and `DEDICATED_XORG` | `arcen_outputs::OutputCapabilities` | Deleted. `physical: true, headless: true` becomes `surface: DedicatedPhysical, headless_capable: true`. `1..=4` becomes the checked region range. |
| `fn supports_region_count` | `arcen_outputs::admits` | Deleted. The gate moves out of the provider's `dry_run` and into the driver, before `preflight`. |
| `DedicatedXorgProvider::dry_run` | `preflight` | Kept, minus the region-count check the driver now performs. |
| `DedicatedXorgProvider` `output: Option<DedicatedXorg>` | `type Binding` | The bound `DedicatedXorg` moves out of the provider into the binding, which `bind` returns. |
| `DedicatedXorgProvider` `committed: bool` and `into_committed` | `OutputTransactionState` and `CommittedOutput::into_parts` | Deleted. The launcher takes the `DedicatedXorg` from the committed output. |
| `commit` (synchronous) | `commit` returning `ready(...)` or `async fn` | Kept. |
| `rollback` returning `Ok(())` after `shutdown()` | `rollback` | Kept, and must satisfy the `DedicatedPhysical` postcondition in section 6. |
| `fn output_provision_error` telemetry mapping in `launcher.rs` | Host-side | Kept, retargeted at the shared error type, with the `Admission` variant added. |
| The module's `#[tokio::test]` lifecycle tests | Split | Generic driver behaviour moves into `arcen-outputs` tests using the in-crate `block_on`. Dedicated-Xorg behaviour stays in the host. |

### Windows transaction driver

| Existing item in `hosts/windows/src/output_provider.rs` and `hosts/windows/src/display.rs` | Destination | Treatment |
| --- | --- | --- |
| `trait OutputProvider` | `arcen_outputs::OutputProvider` | Deleted from the host. |
| `struct OutputProviderTransaction<P>` | `arcen_outputs::OutputTransaction<P>` | Deleted. |
| `fn validate_capabilities` and its formatted strings | `arcen_outputs::admits` plus `CapabilityMismatch` | Deleted. String messages become typed variants. |
| `struct OutputProviderCapabilities` | `arcen_outputs::OutputCapabilities` | Deleted. `exact_modes`, `signed_coordinates`, and `persistent_dedicated_desktop` map one to one. `recovery_rollback: true` becomes `rollback: ExactRestore` on both providers, for different reasons: the physical provider restores the journalled pre-bind topology, and IddCx removes every monitor it created, which leaves the console exactly as it was. The two are then separated by `surface`, `SharedPhysical` for the physical provider and `Virtual` for IddCx. |
| `dry_run(&mut self, plan, session_log_id)` | `preflight(&mut self, plan, context)` | Kept. `session_log_id` moves into `OutputContext`. |
| `bind(&mut self, prepared) -> Result<Binding, String>` | `bind` returning `BindFailure` | Kept, but the internal rollback result is reported as `BindFailure::rollback` instead of being formatted into the message. Applies to both `PhysicalOutputProvider::bind` and `IddCxOutputProvider::bind`. |
| `verify`, `commit`, `rollback` | Same names, future-returning | Kept, implemented with `core::future::ready`. The blocking CCD, NVAPI, IddCx, and settle-wait code does not move and does not become async. |
| `report` and `applied_plan` | `type Evidence` and `evidence()` | Merged into one host-side evidence type holding the `DisplayReport` and the applied `WindowsTopologyPlan`. `applied_plan`'s `Option` default disappears; a provider that has no applied plan says so in its own evidence type. |
| `is_armed` | `is_armed` | Kept verbatim, with the section 6 definition written down. |
| `MultiDisplayTransaction` enum and `MultiDisplayLease` | Host-side | Kept. The enum wraps `arcen_outputs::OutputTransaction<...>` instead of the local one. |
| `MultiDisplayLease::drop` with `block_in_place` | Host-side | Kept. Runtime policy does not move into the shared crate. |
| `MultiDisplayLease::commit` failure leaving the lease armed | Behaviour change | The shared `commit` rolls back on failure, so the lease is already resolved when `Drop` runs. `Drop` keeps its `is_armed()` check. |
| The module's `#[test]` lifecycle tests | Split | Generic driver behaviour moves into `arcen-outputs`; the plan fixtures, capability gate expectations for real Windows plans, and provider behaviour stay in the host. |

### Wayland capability contract

The Wayland seam is not a transaction and must not be forced into one.

| Existing item in `hosts/linux/src/display/wayland.rs` | Destination | Treatment |
| --- | --- | --- |
| Former `trait OutputProvider` with `snapshot()` | Host-local, renamed | Renamed to `WaylandOutputSource` so it stops colliding with `arcen_outputs::OutputProvider`. It stays feature-gated behind `wayland-provider`, stays host-local, and keeps `Error: Error + Send + Sync + 'static`. |
| Former `struct OutputProviderCapabilities` | Host-local, renamed | Renamed to `WaylandOutputCapabilities`. It stays a detection report and never becomes `arcen_outputs::OutputCapabilities`. |
| `detect_output_capability`, `OutputCapabilityReport`, `WaylandOutputUnavailableReason`, `MutterVirtualOutputCapability` | Host-local | Unchanged, including the fail-closed behaviour documented in `docs/architecture/linux-wayland-provider.md`. |

When a native Wayland or Mutter output provider is separately approved, it
becomes a second Linux implementer of `arcen_outputs::OutputProvider`, and it
maps detection facts to shared capabilities like this:

| Wayland detection fact | Shared meaning |
| --- | --- |
| `enumerate_outputs` | Precondition for selecting the provider at all. Not a shared capability. |
| `xdg_output_logical_regions` | Required before the provider may claim `signed_desktop_coordinates`, because logical placement is what makes signed origins meaningful. |
| `fractional_scale` | `fractional_scale`, and only when the provider can authoritatively associate the preference with an output, as `docs/architecture/linux-wayland-provider.md` requires. |
| `mutter_virtual_output: Implemented` | `surface: Virtual` and `headless_capable: true`. |
| `mutter_virtual_output: DetectedButUnimplemented`, `Unavailable`, or `Unknown` | No shared capability. The provider is not selectable. |

Until that work is approved and its dependencies are reviewed, Xorg remains the
only wired Linux provider, and Wayland detection keeps returning an unavailable
reason.

## Consequences

- One trait, one driver, one capability vocabulary, and one error model replace
  three contracts. `hosts/linux` and `hosts/windows` keep their plans, reports,
  journals, watchdogs, and runtime policy, and depend on `arcen-outputs` in the
  allowed direction. Neither host depends on the other.
- The capability gate moves ahead of `preflight` for Linux, which today performs
  it inside the provider. A region count outside `1..=4` is refused before any
  provider code runs, on both hosts.
- Windows gains typed errors. Every existing message that concatenated a primary
  failure and a rollback failure becomes a value with both. Log and telemetry
  wording may change; no wire message does.
- Windows gains an eager rollback when `commit` fails. The final state is the
  same one `Drop` produced before.
- Windows keeps synchronous provider bodies. Adopting the async shape costs each
  provider `core::future::ready`, and it is what lets the Linux dedicated-Xorg
  provider keep its genuinely asynchronous `bind` and `verify`.
- The non-headless invariant of ADR 0009 becomes a postcondition on `rollback`
  and an admission rule on `RollbackGuarantee`, so a provider that cannot prove
  it cannot be admitted for shared physical mutation.
- The IddCx provider of ADR 0008 keeps its default-off gates and its
  handle-owned removal. It changes shape only, not behaviour or signing status.
- The shared crate cannot use `tokio`, so its tests carry a small hand-written
  `block_on`. That is the cost of the purity gate, and it is paid once.
- The three-way name collision on `OutputProvider` is resolved by renaming the
  Wayland seam, which is a host-local, feature-gated, unimplemented interface
  with no callers.

## Out of scope for this ADR

- Any code change. No crate, manifest, workspace member, or host file changes
  because of this ADR.
- The `atomic_start`, `fairness`, and `admission` modules named in the
  consolidation plan. Those are separate decisions and separate work items, and
  the two `spawn_all_or_rollback` copies in `hosts/linux/src/media/multi_capenc.rs`
  and `hosts/windows/src/multi_monitor_capenc.rs` are untouched here.
- Aggregate media plans, bitrate budgets, carrier selection, and client-side
  applied-topology validation.
- Any change to ADR 0009's product invariants or to ADR 0008's virtual display
  decision, both of which this ADR leaves exactly as they are.
- Any protocol, wire, packaging, signing, or release change, and any product
  capability claim.

## Legal and provenance boundary

This ADR introduces no source intake. The implementation it authorises must be
original Arcen code. It must not access, copy, port, or derive from
a local reference corpus, and it adds no third-party dependency. Any future source reuse
remains governed by [`../../legal/ORIGINS.md`](../../legal/ORIGINS.md)
and must be recorded in [`../../legal/ORIGINS.md`](../../legal/ORIGINS.md).
