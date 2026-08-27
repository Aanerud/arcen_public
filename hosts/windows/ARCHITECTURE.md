# hosts/windows — `arcen-pier-windows` (Arcen Pier, Windows)

**Delivery:** Arcen Pier on Windows. Product surface. Binary `arcen-pier`.

## What this is

The native Rust control plane for a Windows workstation host: a QUIC/TLS 1.3 server, Windows
authentication (`LogonUserW`), cold first-login through the Credential Provider or attach
to the authenticated user's interactive desktop session, `SendInput` injection, exact
per-encoder display mirroring with transactional restore, WASAPI system-audio capture, and
capture/encode via the `arcen-capenc` engine — DXGI/WGC → NVENC on NVIDIA hosts, WGC →
source-built OpenH264 on non-NVIDIA hosts (VMware SVGA, Intel-only). No Python,
no ffmpeg.

## How it's grounded

- Lessons encoded:
  - **SID-bound attach plus cold first-login.** A LocalSystem SCM service does
    `LogonUserW(LOGON32_LOGON_INTERACTIVE)`, resolves the authenticated SID to canonical
    `MACHINE\user` / `DOMAIN\user`, and either SID-matches an unlocked WTS session or drives
    the Credential Provider to create or unlock the exact physical-console session. The agent still launches via
    `WTSQueryUserToken` / `DuplicateTokenEx` / `CreateEnvironmentBlock` /
    `CreateProcessAsUserW` on `winsta0\default`.
  - **The inherited-handle AttributeList must OWN its handle array.** `UpdateProcThreadAttribute`
    stores a *pointer* to the handle list; passing a temporary caused `CreateProcessAsUserW`
    to fail `ERROR_INVALID_PARAMETER (0x57)`. The list carries a boxed `_handles: Box<[HANDLE]>`
    that outlives the call. Do not regress this.
  - **Display restore must restore EDID before the original topology.** The injected exact-mode
    EDID can make the journaled mode temporarily invalid. Recovery therefore purges the
    injected EDID and restores the prior effective EDID before replaying the immutable NVAPI
    topology, then completes timing cleanup and verifies the Windows topology/mode before
    disarming `display-recovery.json`. The pre-topology EDID step is idempotent and remains at
    cleanup stage `pending` so a crash deterministically replays it. If the journaled
    `\\.\DISPLAYn` name disappeared, recovery rebinds only to the persisted NVAPI display ID.
    Legacy journals derive that ID by correlating the selected boot-local
    adapter LUID's journaled source geometry/position with the immutable
    Windows/NVAPI topology snapshots before accepting one target. Windows and
    NVAPI source-id namespaces are never compared directly.
    Once an exact display ID is available it is authoritative: recovery never
    accepts a reused boot-local `\\.\DISPLAYn` name first, because that can
    mutate the wrong GPU after reboot.
  - **Recovery disarms only after complete original-topology verification.**
    Journaled `SetDisplayConfig` failure is fatal even if NVAPI timing cleanup
    succeeded. Recovery re-queries all active paths and modes and requires the
    exact captured stable NVIDIA display identities, path set, source
    positions/primary, target modes, and active-output set before removing the
    journal.     Only boot-local adapter LUID and source/target mode indices/numeric
    identifiers are normalized. Clone-group IDs remain authoritative, and each
    target's referenced desktop-image mode (source size, desktop region, clip)
    is compared by value; every other path/mode byte remains authoritative. A
    semantic mismatch or unavailable verification retains the journal and keeps
    the service fail-closed for operator recovery.
    Journal v4 stores one tagged backend binding per active path. NVIDIA
    bindings carry immutable PCI adapter/connector identity plus authoritative
    NVAPI display ID, one-bit physical output ID, and head; Windows-native bindings carry the stable adapter/device
    interface/connector/full-EDID identity and never require NVAPI. Recovery
    selects only the recorded backend—there is no cross-backend fallback.
    NVIDIA recovery maps every persisted display ID (including connected but
    inactive outputs) through current NVAPI and QDC all-path inventory before
    rebuilding the complete original path/clone grouping. NVAPI application
    leaves every NVIDIA `sourceId` zero so the driver assigns per-adapter CCD
    IDs; the all-path inventory still proves every source/target association.
    Windows-native recovery reconstructs
    and verifies Windows topology directly. Legacy v1-v3 journals still parse,
    but topology-changing automatic recovery fails closed because they cannot
    prove every output identity.
    An output-bound EDID-attempt stage and intended full-EDID hash are durably
    synced before `NvAPI_GPU_SetEDID`, and full-byte readback must match before
    the stage advances. If an Arcen EDID was installed before a crash, initial NVIDIA rebind ignores
    mutable EDID fields only after immutable GPU/display/connector proof; after
    cleanup the full original EDID hash (including serial) must match.
    The guarded `migrate-display-journal` maintenance command is the only
    legacy upgrade path: elevated exact local-console administrator, QDC
    all-path plus connected NVAPI inventory, atomic v4 publication, then normal
    exact restore. Ambiguous inactive outputs or mutable-EDID-only matches leave
    the legacy journal untouched.
    Before Windows-native recovery mutates anything, every journaled path is
    rebound through current `QDC_ALL_PATHS`, DXGI, device-interface, connector,
    and full-EDID identity. Only a newly reconstructed path/mode array with
    current LUID/source/target identifiers is submitted; persisted `DISPLAYn`
    and CCD identifiers are never replayed. NVIDIA and Windows-native tags are
    exclusive, so failure of NVIDIA mapping or mandatory timing/full-EDID
    cleanup retains the journal and fails recovery rather than falling through.
    Version-4 journals explicitly carry `mutation_started`, selected-path
    binding, every backend identity, and all NVAPI EDID/timing stages and
    fingerprints. Missing authority fails before deletion or mutation.
    Legacy migration is limited to explicit mutated, durable `nvapi: null`
    evidence whose every current adapter is independently proven non-NVIDIA;
    legacy NVIDIA/unknown EDID ownership is preserved for manual recovery.
  - **WGC fallback:** DXGI Desktop Duplication delivers no frames on headless NVIDIA vGPU/RDP;
    Windows.Graphics.Capture (DWM-based) is the fallback path. Handled in `arcen-capenc`.
  - Anonymous-pipe WebSocket IPC broker↔agent with handle-list-restricted inheritance;
    kill-on-close job object; `BrokerAgentLease` singleton.
    - **Logging handles are transactional.** The broker and user-session agent
      write through one lock-backed file each. Reopen opens the replacement first,
      serializes writers, rolls stdout back if stderr rebinding fails, and swaps
      file ownership only after both `SetStdHandle` calls succeed.

## Rules — what it must be (invariants)

1. Depends on `arcen-capenc`, `arcen-input`, `arcen-media`, `arcen-protocol`,
   and `arcen-telemetry`; spawns `current_exe() capenc` as a subprocess. Never
   depends on the Linux Pier or the gateway.
2. Windows-only deps (`windows` crate features) behind `cfg(windows)`; protocol/transport
   unit tests still run on the dev box.
3. `unsafe` allowed (~151 sites: Win32 FFI) but scoped.
4. Identityless physical-console LogonUI in WTS `Active` or `Connected` state
   means cold first-login through `CPUS_LOGON`. A single SID-matching locked
   `Active` physical-console session uses
   `CPUS_UNLOCK_WORKSTATION`, but only when every additional candidate is an
   identityless disconnected service/listener entry. Another user, same-SID
   stale/RDP session, connected identityless session, unknown protocol, or
   ambiguous session topology fails closed independent of enumeration order.
5. Own direct-QUIC UDP port `18444`;
   production layout uses
   `%ProgramFiles%\Arcen\Pier` and `%ProgramData%\Arcen`. One rustls
   (tokio-rustls + tokio-tungstenite), with PEM-only certificate material.
6. Persist host settings in `%ProgramData%\Arcen\pier.json`; explicit CLI flags override the
   file. Select desktop/capture by exact DXGI adapter description + adapter-local output, and
   keep NVENC on that same GPU for zero-copy. Resolution happens in the user-session agent,
   not session 0; `validate-config` provides a non-mutating interactive check.

The common Windows/Linux schema and Windows platform section are documented in
[`../../docs/operations/pier-configuration.md`](../../docs/operations/pier-configuration.md).

## Interfaces / boundaries

- **Consumes:** `arcen-input`, `arcen-media`, `arcen-protocol`; embedded `arcen-capenc` through a Pier subprocess; Win32 (WTS, LogonUser,
  SendInput, WASAPI, NVAPI, DXGI/WGC).
- **Exposes:** QUIC on UDP `:18444`.

## Module map (from the proven source)

- `main.rs` — entry (broker vs agent role).
- `config.rs` — strict JSON schema, relative-path resolution, defaults + CLI override merge.
- `service.rs` — SCM dispatcher, LocalSystem service status/control, graceful stop.
- `tls.rs` — bounded opened-handle PEM loading and ACL enforcement, rustls
  posture/certificate validation, atomic reload, and expiration state.
- `log_maintenance.rs` — bounded recognized-file enumeration and host actions
  over the pure `arcen-telemetry` maintenance plan.
- `logon_activation.rs` — active-console lookup + System32-only `SendSAS`.
- `windows_session.rs` — WTS attach, token duplication, `CreateProcessAsUserW`, the
  handle-owning AttributeList.
- `auth.rs` — `LogonUserW`, SID match, lock-state check.
- `ipc.rs` — anonymous-pipe broker↔agent session IPC, job object.
- `session.rs` — session/agent lifecycle.
- `resume.rs` — process-owned ring HMAC key, direct-resume bindings, atomic
  one-slot generation rotation, and replacement QUIC handoff.
- `capenc.rs` — spawn + supervise the Pier's `capenc` subprocess; `EncoderSelection` (auto resolved against
  the configured adapter before spawn).
- `encoder_admission.rs` — bounded aggregate NVENC/MF candidate planning and
  shared measured-admission integration. It rechecks the operator adapter
  allowlist, preserves each output's stable `(adapter_luid, target_id)`
  identity, gives required YUV444 regions hardware priority, and never lets a
  measurement adapter auto-borrow another GPU. Before streaming starts, the
  production adapter freshly resolves each stable output and concurrently runs
  finite `admission-v1` children for the exact candidate bindings; the session
  applies only `selected_specs` and rejects atomically when no measured set
  passes. Native measurement adapters may be capability-gated; the pure planner
  and injected traces are portable.
- `gpu_probe.rs` — `diagnose-host` inventory: adapters, outputs, D3D11/video capability,
  encoder runtimes, same-adapter recommendation. Non-mutating.
- `audio.rs` — WASAPI loopback capture.
- `clipboard.rs` — bounded transport queue plus the user-session
  `HWND_MESSAGE`/`AddClipboardFormatListener` adapter.
- `display.rs` / `edid.rs` / `nvapi.rs` / `recovery.rs` — the physical `DisplayPolicy` transaction
  (exact mode + topology isolation for NVENC, negotiated ladder for MF/VMware), full-snapshot
  restore, EDID generation/purge, display-recovery journal + crash watchdog. All of it exists
  because the fallback host path mutates the *physical* display and must put it back.
- `multi_monitor_topology.rs` — the pure physical multi-monitor planner: stable
  `(adapter_luid, target_id)` output inventory/binding, exact CCD/NVAPI mode and
  rotation matching, the bounding-desktop ceiling, and plan→region-set
  construction. The rotation-aware footprint math, edge-aware mixed-scale
  placement, origin policy, and region-set construction it uses live in the
  shared `arcen_media::topology_placement` module. This host passes
  `TransformConvention::NativeNeedsTransform` (outputs are driven at their
  native pre-rotation mode and Windows applies the rotation) and
  `OriginPolicy::PreserveSigned` (the Windows virtual desktop is natively
  signed and the OS anchors the primary at `(0, 0)`, so a layout is never
  translated). Monitors that logically touch are placed flush against their
  own neighbor's footprint, so a mixed-scale chain never accumulates a gap or
  an overlap; a monitor with no touching path to the primary keeps its
  absolute offset at the primary's scale.
- `output_provider.rs` / `iddcx.rs` / `iddcx-provider/` /
  `driver/arcen-iddcx/` — the additive first-party IddCx provider: pure ABI and
  capability contracts, exact adapter-affinity resolution, inherited
  broker-to-agent control ownership, dynamic one-to-four monitor
  create/configure/depart, verified output rebinding, swapchain draining, and
  explicit/final-handle rollback. Both the IddCx and multi-monitor config gates
  default off, and a missing or under-capable driver withholds the pre-auth
  offer. The source compiles unsigned with WDK 10.0.26100, but no driver is
  packaged, signed, installed, or deployment-approved. See
  [`../../docs/adr/0008-virtual-display-for-windows-hosts.md`](../../docs/adr/0008-virtual-display-for-windows-hosts.md).
- `timezone.rs` / `tz_map.rs` — opt-in broker-owned system time-zone redirection,
  CLDR IANA→Windows mapping, exact dynamic-rule snapshots, phased recovery journal,
  and broker-death watchdog.
- `input.rs` — `SendInput`; absolute move+edge and move+wheel remain atomic,
  while relative movement uses `MOUSEEVENTF_MOVE` only and relative edges omit
  the absolute warp. Also owns `PenInjector`: an RAII wrapper around one
  Windows 10 1809+ synthetic `PT_PEN` pointer device
  (`CreateSyntheticPointerDevice`/`InjectSyntheticPointerInput`/
  `DestroySyntheticPointerDevice`) for typed pen input — see "Typed
  pen/tablet input" below.
- `logging.rs` / `latest.rs` — reloadable tracing (`arcen::<area>`), latest-frame
  bookkeeping.
- `eventlog.rs` — best-effort Windows Event Log lifecycle sink: pure bounded
  insertion-string formatting, an injectable `Win32EventLogApi` FFI backend
  (`RegisterEventSourceW`/`ReportEventW`/`DeregisterEventSource`), the
  process-local `LifecycleEmitter`, and the bounded `query_recent_events` seam
  consumed by Support Bundle. See "Lifecycle Event Log" below.
- `support_bundle.rs` — service-independent, bounded ZIP collection over strict
  config, recovery, managed-log, diagnostics, and sanitized Event Log allowlists.
- `AuthResponse.session_log_id` is validated before authentication continues;
  one diagnostic-only `sid` follows broker → WTS session agent → capenc. Missing
  or invalid old-client values receive a host UUID fallback and never affect SID
  binding or authorization.
- Optional disclaimer content is prepared once by the broker before service
  readiness from strict `auth.disclaimer` config. The default source is
  `%ProgramData%\Arcen\disclaimers\<locale>.txt`; a relative directory resolves
  from `pier.json`. Missing, empty, invalid UTF-8, oversized, or unsafe locale
  input fails startup and `validate-config`. The prepared text remains
  broker-only and is never copied into session-agent IPC.
- When enabled, `session.rs` requires the exact lowercase SHA-256 acknowledgment
  before `LogonUserW` or Credential Provider handoff. It records bounded
  locale/digest/time/correlation evidence only after OS authentication succeeds.
  No banner text or account identity is logged, and no claim is added to strict
  `SessionGrantClaims` v1.

  ## Clipboard redirection

  Trusted strict JSON/CLI policy is carried in `AgentConfig` to the per-session
  agent. LocalSystem terminates TLS/authentication and blindly relays bounded
  WebSocket frames; it never calls clipboard APIs. Exact clipboard v1 plus client
  capability bits is required before the agent starts its dedicated clipboard
  thread on `winsta0\default`.

  The thread owns a private message-only window, listener registration, exact
  5/10/20/40/80/80/80/80 ms OpenClipboard retries, sequence-number deduplication,
  and private `ArcenClipboardOrigin` marker. Reads prefer `CF_UNICODETEXT`, then
  strict `CF_DIBV5`; writes set the marker first and transfer movable global-memory
  ownership only after successful `SetClipboardData`. A close guard covers every
  successful open. Thread/async crossings use capacity-one latest slots and
  pointer-free wake messages.

  Network receive validates policy before offers, before reassembly growth, and
  after UTF-8/PNG validation. The writer keeps one active clipboard transfer and
  one pending replacement, sends one chunk per scheduling turn, and interleaves
  clipboard with control/media. External and broker-agent tungstenite limits are
  exactly one clipboard header plus one 1 MiB chunk; the 16-connection pre-auth
  bound is asserted in tests.

## Relative input and cursor capture

Every nonzero keyboard, absolute, relative, button, and wheel message passes
through one shared `InputSequenceTracker` before `SendInput`; legacy zero remains
unsequenced. Host cursor mode is fixed by authenticated setup and bound into the
resume topology. The repeated `ClientHello` must match before attachment capture
starts.

Local mode preserves DDA-first/WGC-fallback behavior and requires WGC cursor
exclusion when WGC is selected. Host mode never selects DDA: capenc forces WGC,
and `SetIsCursorCaptureEnabled(true)` is a strict startup requirement for both
NVENC/WGC and MF/WGC. Pier confirms Host only after the first encoded frame.
DDA pointer-shape composition is not implemented or advertised.

## Typed pen/tablet input

Full design: [`../../docs/architecture/pen-tablet-input.md`](../../docs/architecture/pen-tablet-input.md).
Operator validation: [`../../docs/operations/pen-tablet-input.md`](../../docs/operations/pen-tablet-input.md).

`input.rs::PenInjector` is an RAII wrapper around one Windows 10 1809+
synthetic `PT_PEN` pointer device created with the public
`CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)` /
`InjectSyntheticPointerInput` / `DestroySyntheticPointerDevice` API — not
`SendInput`, and **no Wintab claim is made**. Pressure maps to the
documented `0..=1024` Windows Ink pointer-pressure range
(`pen_pressure_to_windows`) even when the source tablet reports more levels;
rotation maps to `0..=359` and tilt to `-90..=90`; eraser/barrel state maps
to `PEN_FLAG_ERASER`/`PEN_FLAG_BARREL` in `POINTER_PEN_INFO`. A small,
unit-tested `PenPointerState`/`PenPointerEdge` state machine drives the
correct `POINTER_FLAG_INRANGE`/`_DOWN`/`_UPDATE`/`_UP`/`_INCONTACT`
combination for each hover/contact transition. `pen_pixel_location()` reuses
the same validated selected-output geometry as mouse
`POINTER_INFO.ptPixelLocation` — pen is never routed through mouse
`SendInput`. The handle and state live in the interactive user-session
agent, never the LocalSystem service.

`build_server_hello` advertises every pen capability field
(`pen`/`pen_pressure`/`pen_tilt`/`pen_rotation`/`pen_eraser`/
`pen_proximity`) **together, all-or-nothing**: the synthetic `PT_PEN` device
either exists with every axis real from the same `POINTER_PEN_INFO` sample,
or `CreateSyntheticPointerDevice` failed (older Windows, Windows Ink
disabled, or a transient API failure) and every field advertises
`Unavailable` — the attachment still proceeds mouse-only, never a partial or
fake pen capability. Contact/proximity is released on disconnect, reconnect
detach, focus reset, or injector drop. No Wacom driver is installed or
required on this Windows host.

## Observability, rotation, and runtime profiles

`pier.json` carries canonical cumulative `logging.level` (`0` Critical through
`3` Debug), QoS targets, and retention days. Production/package configuration
defaults to Level 0; legacy `logging.verbosity` is accepted for one release by
the shared resolver and cannot coexist with `logging.level`. Windows-only
active-file rotation lives at `platform.logging.rotate_mb`.
Retention is normalized to 7–100 days by `arcen-telemetry`; rotation defaults
to 32 MiB.

The broker writes `%ProgramData%\Arcen\logs\arcen-pier.log`; correlation-named
agents write `logs\sessions\arcen-session-agent-<sid>.log`. Both are canonical
JSON Lines emitted through `arcen-observability`; broker, session-agent, and
diagnostic processes retain their installed runtime for process lifetime.
Orderly SCM, console, maintenance, diagnostic, and agent exits invoke the
repeatable bounded flush (three seconds per sink) before process termination;
workers remain live until the process actually exits.
Lifecycle records also retain exact `ArcenPier` provider IDs in the bounded
64-entry `Application` Event Log sink. Maintenance runs
before sessions are accepted and every 24 hours, archives active files under
`logs\archive`, and deletes expired recognized regular files. Reparse points
and unrelated files are never included. The sessions directory remains
broker-only; each active file receives a write-only ACE for that session's
account SID, which is removed on archive or session close.

SCM control 200 atomically enables temporary Level 3; control 201 re-reads the
configured profile, QoS targets, and `ARCEN_LOG` and propagates them to the
active agent. Control 202 only enqueues a TLS reload;
orchestration securely reads and validates the configured PEM replacement
outside the resolver lock, then atomically swaps it for new handshakes. Existing
sessions retain their negotiated key. Reload failure retains the last good
still-valid certificate; after expiration the resolver refuses new handshakes
without plaintext fallback. A bounded latest-wins private message carries
filter reload and file reopen to the one active agent. The broker rejects that
reserved message type from client-originated traffic, so this is not a product
wire change.

The host consumes optional client QoS/network facts on the existing health
ping, computes host/client/overall health with shared targets and two-window
hysteresis, and emits transitions plus 60-second service/session snapshots.
Absent legacy-client telemetry remains unavailable. `GetAdaptersAddresses`
supplies normalized ethernet/wifi/vpn/loopback/other link rate and MTU facts;
SSID is not collected without a future explicit local disclosure policy.

The pier-windows.example.internal development deployment currently overrides the package default
to Level 2 for diagnosis. That is an environment-specific deployment setting,
not a production default and is not baked into `pier.json`.

## TLS lifecycle

Pier builds and validates the entire TLS lifecycle before binding UDP 18444 or
reporting service readiness. The source is an administrator-managed PEM chain
and one matching PKCS#8/PKCS#1/SEC1 key. QUIC uses TLS 1.3 through the ring
cryptographic provider and a 30-day expiry warning. Configured expected DNS/IP SANs
must all match exactly, making stricter SAN configuration an intentional
rollout break rather than a fallback.

The loader uses opened file handles, bounded reads (1 MiB chain, 128 KiB key),
stable handle metadata, no reparse/device traversal, and a protected key DACL
containing only SYSTEM and Administrators. It never reopens by path after
validation. Lifecycle telemetry carries only bounded hashes, key
algorithm/size, expiry state, and closed reason classes.

Enterprise PKI owns issuance and rotation. The SMB helper is an
absent-only, transactional ECDSA P-256 bootstrap; `-Renew` preserves the SPKI
and `-ForceNewKey` is an explicit trust change. The service never issues or
rotates. Windows certificate-store and CNG keys, runtime certificate fetch,
mTLS, 0-RTT, and plaintext fallback are outside this boundary.

## Direct session auto-reconnect

`auth.reconnect_window_secs` configures an inclusive `0..=7200` second direct
reconnect window and defaults to 1200; zero disables advertisement, grant
issuance, and detached retention, so attachment/final cleanup begins
immediately. Replacement attachments arrive over QUIC. This adds no
Gateway/Span, OIDC, entitlement, licensing, mTLS, 0-RTT, network fetch,
protocol-version bump, or durable resume state.

The broker creates one process-owned, one-session resume registry before TLS
initialization or listener bind. It generates one 256-bit `getrandom` key,
keeps the raw key only in zeroizing process memory, and uses `ring`
HMAC-SHA-256 over `arcen-identity`'s domain-separated canonical
`DirectResumeGrantClaims`. Tokens bind the validated TLS SPKI HostIdentity,
WTS session, canonical account SID, Deck holder nonce, disclaimer
digest/version, generation/nonce, and bounded time. The registry also retains
the exact authenticated Deck topology/time-zone digest and current held output
binding. Neither key nor token is logged, persisted, sent as a key, or derived
from TLS/password material.

Initial `LogonUserW`/Credential Provider behavior is unchanged. Resume opt-in is
honored only after SID+WTS bind, disclaimer acceptance, `BrokerAgentLease`,
time-zone ownership, user-session agent readiness, and display lease/journal
ownership are fixed. Only then is generation 1 installed and returned in
`AuthResult`. While attached, one loop refreshes the single slot every `W/2`
and sends the `2W` successor as an ordered in-band `AuthResult` before further
media. Rotation precedes send and has no acknowledgment, so lost delivery
leaves the old Deck token a replay. A known successor send failure drains
immediately; otherwise an authenticated, unexpired exact predecessor on the detached slot is
rejected as `replayed` and atomically enters the same final drain. No path
reopens the predecessor or reveals the successor, and normal visible
authentication can proceed after final cleanup releases ownership.
Generation/signing failure also drains terminally. While that slot is detached,
the next connection receives a
resume-only-capable request without re-presenting the disclaimer; this allows
the token's already-fixed disclaimer evidence to be verified without another
acceptance. A normal initial connection still receives and must acknowledge
the configured disclaimer.

`method=resume` is credential-free and branches to the registry before any
Windows authentication or Credential Provider call. Under the registry mutex,
HMAC and every retained binding are validated and the exact generation+nonce
is compare-and-rotated. One concurrent attempt wins; replay receives a stable
bounded `AuthResult.error_code`. The broker then re-observes the same unlocked
WTS account and SID, asks the held user-session agent to verify the exact
output geometry/device, and sends the successor grant before allowing a new
attachment to start. Each attempt has a fresh Session Log ID and records the
previous diagnostic ID when available; that ID is never authorization.

Unexpected EOF/reset/transport failure first marks the registry slot detached,
then sends a private detach command. A concurrently verified handoff can wait
in the broker owner channel while agent cleanup completes;
detach-command/status failure falls through the same terminal revoke/final
cleanup path. The
agent's **attachment cleanup** closes input (including modifier reset),
capture/encode, audio, writer, and attachment queues, but retains its broker
IPC/process, display lease and watchdog/journal. The broker retains the
time-zone lease,
`BrokerAgentLease`, WTS principal, and one monotonic generation timer. Accepted
resume reuses those leases, performs a fresh capability exchange, creates
fresh attachment media resources, and forces an IDR before streaming. It never
reapplies or rearms display/time-zone state. The exact reconnect window starts
at unexpected loss, not grant issue; failed resume transports preserve it.
Every awaited client or private-agent send in an opted-in attachment is bounded
to `min(5 seconds, W/2)` (at least one millisecond), so a blocked write cannot
starve the biased refresh interval; legacy attachments retain the existing
timeout.

WebSocket Close, timer expiry, WTS lock/logoff/SID change, output or TLS
identity change, agent IPC failure/crash, broker shutdown, successor delivery
failure, and authenticated exact-predecessor replay on a detached slot are terminal.
They revoke/remove the slot and stop attachment resources first. **Final
cleanup** then closes and bounded-waits the agent; the agent restores display
while its existing watchdog/journal is still authoritative, and only after
agent exit does the broker restore time zone and release `BrokerAgentLease`.
Repeated terminal events and stale timers are no-ops; display/time-zone
journals are disarmed only by their existing successful restore paths.
Agent-crash watchdog ordering remains unchanged through detachment.

The owning contract and complete ordering are in
[`../../docs/architecture/session-auto-reconnect.md`](../../docs/architecture/session-auto-reconnect.md).
Graduation requires Shared/Architecture, Release/Security, macOS Client,
Windows Host, and Linux Host review.

## Deskside physical-workstation privacy

Deskside is disabled unless strict `platform.desktop.deskside` configuration is
present.
Before any display mutation, the authenticated `winsta0\default` agent proves
that its WTS session is the active physical console with local protocol, probes
fresh CCD/DXGI inventory, rejects every software/basic/remote/indirect/
paravirtual or unknown active output, requires CPUID no-hypervisor plus a pinned
positive normalized SMBIOS system/chassis hash, matches every non-capture
physical output to unique operator-provided identity and CCD EDID-tuple hashes,
and binds the distinct indirect-wired capture output to a pin plus runtime
adapter LUID/source/target identity. Adapter classification is not tainted by a
single indirect output, so mixed hardware adapters remain representable while
virtual-only adapters refuse. Missing or conflicting pins refuse authentication
before streaming.

The agent then drives `arcen-session::deskside::DesksideProtection`: a dedicated
native thread installs `WH_KEYBOARD_LL` and `WH_MOUSE_LL`, passes injected and
lower-integrity-injected flags only when their process-random `dwExtraInfo`
marker matches Arcen's injector, swallows physical or foreign injected
keyboard/mouse, and runs a real message pump. `BlockInput` is never used. Hook
liveness is proven for keyboard and mouse separately by bounded, swallowed,
process-random canaries; SendInput failure, missed heartbeat, hook-thread exit,
or evidence-bound topology change immediately drains the session during both
streaming and reconnect hold. The existing
`ExactIsolated` transaction deactivates the pinned local outputs while preserving
the capture target. A host that cannot prove exact isolation refuses; no
capture-output overlay is accepted as protection.

The existing version-3 display recovery journal carries optional bounded
Deskside stage and fingerprint metadata. It remains backward-compatible, uses
the existing display watchdog, and rejects symlink/reparse paths. The installer
must retain SYSTEM/Administrators-only ACLs. During resumable detach the same
agent, hooks, display lease, and journal remain alive; accepted resume does not
rearm them. Terminal cleanup stops attachment input/media, restores display,
then releases hooks even when display restore fails. A failed display restore
retains the journal for watchdog/operator recovery.

This is not a kernel HID filter. It does not block SAS/secure desktop, kernel
HID, pen/Wacom, unsupported input classes, or a hostile process already running
on the same interactive desktop that can observe and replay injected-event
metadata. The process-random marker prevents accidental/foreign injection from
being trusted; it is not an OS access-control boundary. Process death removes hooks;
topology restoration depends on the existing watchdog. Hot-plug, sleep/resume,
driver-reset, and real multi-monitor privacy remain mandatory physical-lab
release gates.

## Lifecycle Event Log (2026-07-19)

`arcen-telemetry::LifecycleEventKind`/`ValidatedLifecycleEvent` (IDs
1000-1404) are reported best-effort through a process-local Windows Event
Log source, additive to the existing `tracing`/file-log paths above — never
a replacement. `eventlog.rs` separates concerns:

- A pure `build_insertion_strings` formatter renders one bounded, deterministic
  summary plus sorted `key=value` fields (`event_id`, `event_name`, `category`,
  `outcome`, `severity`, `correlation_id`, and every schema-approved field) —
  no user SID, raw OS error, or credential ever appears.
- `Win32EventLogApi` abstracts `RegisterEventSourceW`/`ReportEventW`/
  `DeregisterEventSource` so tests inject a fake backend instead of touching
  the real `Application` channel.
- `LifecycleEmitter` is process-local RAII: one source registration per
  process, released on `Drop`. `emit()` never returns an error and never
  blocks or changes an auth/session/display/CP outcome; a formatting or FFI
  failure is reported once via `tracing::warn!` and then silently ignored.
- `query_recent_events` is a bounded (500 records / 4 MiB), newest-first Event
  XML read, independent of the live emitter. Support Bundle strips native
  computer and security identity fields before archiving the excerpt.

Each process registers its own sink: the broker (service or console mode),
the per-session agent, the `restore-display` CLI, and the crash watchdog.
Service start/stop/failure (1000-1002) and session/CP auth outcomes
(1100/1101/1300/1301) come from the broker, using the already-validated
`session_log_id` correlation. Stream start/end/interruption (1102-1104) and
in-process display arm/restore (1200/1201/1203) come from the per-session
agent, on the same correlation. The standalone `restore-display` CLI and the
crash watchdog (1201-1204) run in independent processes with no live session,
so they use a freshly generated correlation id instead — a disclosed,
narrower gap versus the live-session events above.

Registration is separately owned: `packaging/windows/host/eventlog-source.ps1`
installs/uninstalls the `ArcenPier` Application-channel source (see
[`INSTALL.md`](INSTALL.md)), independent of the Credential Provider's
`registration-common.ps1`. v1 ships no compiled message DLL.

## Support bundles

`arcen-pier support-bundle [--out <DIR>]` dispatches before configuration,
logging, SCM, TLS, or server startup, so it remains usable when the service
cannot start. The default output is `%ProgramData%\Arcen\Support`; failure to
create or write that directory is explicit and requires `--out`.

The collector uses the existing managed-log inventory, `recovery::default_path`,
read-only SCM state, DXGI diagnostics, and bounded Event Log query. It streams
with a fixed 64 KiB buffer, caps included logs at 200 MiB and total payload at
256 MiB, rejects reparse points, publishes through a same-directory partial
file, and records source failures as typed manifest notices. Configuration is
recursively redacted. Neither configured TLS certificate/private-key path nor
file is inspected, opened, hashed, or recorded; bounded TLS lifecycle metadata
can appear only through already-sanitized logs/events. Archive names and
manifests never contain the hostname.

## Time-zone redirection

Time-zone redirection is disabled by default and is enabled with
`redirection.timezone: true` or `--timezone-redirection`. It is a
**system-wide** Windows mutation performed only by the LocalSystem broker with a
scoped `SeTimeZonePrivilege`; it never claims to be per-WTS-session and never
uses the linked/user token. The existing `BrokerAgentLease` serialization means
only one authenticated agent can own this machine-wide state.

The authenticated `AuthResponse.timezone` is authoritative. A later
`ClientHello.timezone` mismatch is logged but cannot change the decision.
Before mutation, Pier writes
`%ProgramData%\Arcen\recovery\timezone-recovery.json` in the installer-protected
SYSTEM/Administrators-only recovery directory, rejects reparse points in
existing path components before privileged access, starts an inherited
parent/ready-handle watchdog, and advances durable armed/applying/applied and
restoring/restored phases around exact `DYNAMIC_TIME_ZONE_INFORMATION`
snapshots. Normal agent shutdown restores before releasing the broker lease.
Startup reconciliation runs even when redirection is disabled.

If current state matches neither journal snapshot, Pier marks the journal
conflicted, retains it for operator review, disables only further time-zone
redirection, and continues serving sessions. The operator may inspect bounded
metadata in a support bundle and run
`arcen-pier restore-timezone [--journal <PATH>]` after resolving the conflict.

The generated map is built only into `OUT_DIR` from Unicode CLDR 48.2
`windowsZones.xml` (tag `release-48-2`, commit
`11299982335beb974c1c63c45265184e759c0f41`). The data is © Unicode, Inc. and
licensed under the Unicode License v3 (`Unicode-3.0`):
<https://github.com/unicode-org/cldr/blob/release-48-2/LICENSE>.

## Credential Provider — connect→login (no autologon) — INSTALLER MUST BUILD THIS IN

Autologon is **not** a product mechanism: it breaks account switching, dies on a cold
boot after a crash, and **AD-joined workstations have no autologon at all**. The real
session-creation path is the **Winlogon Credential Provider** using the same operating-system Credential Provider path that commercial remote-desktop hosts use for the Winlogon secure desktop.

**Components**
- `hosts/windows/cp-ipc` (`arcen-cp-ipc`) — pure-Rust CP↔broker framing + a **sealed-
  credential envelope** (X25519 + HKDF-SHA256 + AES-256-GCM via `ring`), bound to a
  per-Advise transcript. No plaintext credential ever crosses the pipe.
- `hosts/windows/credential-provider` (`arcen-credential-provider`) — the COM
  `ICredentialProvider` DLL (**cdylib** `arcen_credential_provider.dll`) that runs inside
  LogonUI, receives the sealed credential over a SYSTEM-only named pipe, unseals it, and
  submits it to Winlogon. CLSID `{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}`.
- Host side (`arcen-pier-windows`): `cp_pipe.rs` (SYSTEM-only control pipe, SDDL ACL),
  `first_login.rs` (Ready/Session timeouts, SID-bind poll, single-flight), wired into
  `session.rs` (`CpCoordinator`) + `auth.rs`/`windows_session.rs`.

**Flow:** Deck authenticates → an identityless `Active`/`Connected`
physical-console LogonUI uses `CPUS_LOGON`; one exact SID-matching locked
`Active` physical-console session uses
`CPUS_UNLOCK_WORKSTATION` → the broker seals the credential to a fresh CP peer in
that same session and scenario → Winlogon creates/unlocks it → host rebinds the
same console session by the original authenticated SID → streams.

**Manual delivery contract (implemented in [`INSTALL.md`](INSTALL.md)):**
1. **Build the CP for MSVC, signed for production.** `build.cmd` produces static-CRT
   `x86_64-pc-windows-msvc` artifacts with no MinGW runtime staging. `install.ps1` enforces
   the signer; `install-test.ps1 -IUnderstandThisModifiesWinlogon` is the unsigned lab path.
2. **Install layout:** DLL → `%ProgramFiles%\Arcen\CredentialProvider\arcen_credential_provider.dll`.
3. **Register (64-bit hive):**
   - `HKLM\SOFTWARE\Classes\CLSID\{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}\InprocServer32` = DLL path, `ThreadingModel=Apartment`.
   - `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}`.
   (`registration-common.ps1::Install-ArcenCredentialProvider` does this; `uninstall.ps1` rolls back.)
4. **Run `arcen-pier.exe service ...` as the automatic LocalSystem `ArcenPier` SCM service.**
   The service normally receives only `--config "%ProgramData%\Arcen\pier.json"`.
5. **Do NOT ship autologon.** Disable `AutoAdminLogon`/`ForceAutoLogon`; the CP is the login path.

## Direct cold-boot first-login — CP path proven; media retest pending

The missing behavior was resolved without a separate logon-driver executable or undocumented
Winlogon API:

1. Pier runs as a real LocalSystem SCM service, so documented `SendSAS(FALSE)` is valid.
2. The broker recycles stale CP connections, sends SAS, then accepts only a fresh
   `CPUS_LOGON` Ready whose reported PID and kernel session match the physical console.
3. The broker prevalidates the account with `LOGON32_LOGON_INTERACTIVE`, resolves the SID to a
   canonical Windows name, and seals it to that CP instance.
4. The CP protects the password with `CredProtectW` and returns a checked
   `KERB_INTERACTIVE_UNLOCK_LOGON` / Negotiate serialization. Armed plaintext expires after
   30 seconds independently of the longer profile/session wait.
5. Pier rebinds only by the authenticated SID, never by the CP-provided name.
6. The broker revalidates the selected session immediately before dispatch and
   accepts the result only from that same physical-console session. Existing
   unlocked attach is unchanged; another user/session, RDP, disconnected/stale
   state, an old CP generation, or an ambiguous topology cannot select a CP path.
7. After CP success, the exact SID/session must remain continuously active and
   unlocked for 15 seconds before the agent creates its WGC item. This avoids
   binding capture during the LogonUI-to-Explorer/DWM transition; any transient
   lock, identity/session change, or console change resets the stability gate.

**Live evidence on `pier-windows.example.internal`:** two separate no-user reboots produced
`GetCredentialCount(autologon=true)`, `GetSerialization(FINISHED)`,
`ReportResult(STATUS_SUCCESS)`, Security 4624 Logon Type 2 with `Logon Process: User32`, an
active console WTS session, and successful SID rebind/agent launch. An invalid password was
rejected before CP handoff; the next valid attempt reached the Deck's `streaming` state.

The CP/authentication path remains proven independently of media acceptance.
Cold-login end-to-end success still requires a manual stream retest after the
post-login capture lifecycle gate.

## Exact client display mirroring — per-encoder policy (2026-07-17)

The product contract is the Deck's Displays settings page: the host recreates the client's
monitors — no ghost displays. Exactness is an encoder property, so the display transaction
in `display.rs` takes a `DisplayPolicy`:

- **`ExactIsolated` (direct-NVENC hosts):** the exact client mode on the configured output
  or the session is refused — no fallback modes. After the (NVAPI or DEVMODE) apply
  settles, `isolate_session_output` re-queries the active topology
  (`QDC_VIRTUAL_MODE_AWARE`), deactivates every non-session path, moves the session source
  to (0, 0) — the GDI primary — and applies with `SDC_APPLY |
  SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_VIRTUAL_MODE_AWARE` (never
  `SDC_SAVE_TO_DATABASE`). The transaction succeeds only when DXGI confirms exactly one
  active output at (0, 0) with the client size — so Microsoft Basic `DISPLAY1` is OFF for
  the session (the hypervisor console blanks; expected) and apps open inside the capture.
- **`Negotiated` (OpenH264 / virtual hosts):** software H.264 cannot promise
  exact display retargeting on every hypervisor, so these sessions fit requests
  within the shared 1920x1200p30/even-dimension contract and use supported modes;
  an unaligned current mode cannot become the terminal fallback. The applied size is
  honestly reported in ServerHello. Topology is left as-is.

The shipped Windows graph is NVENC plus source-built OpenH264. Media Foundation
remains an optional standalone comparison feature in `arcen-capenc`, but is not
accepted by Windows Pier configuration or compiled into the product graph.

For an exact isolated lease, `ServerHello.supports_display_update` is true only
when the live display backend also proves an owned NVAPI exact-timing snapshot
and rollback-capable retarget path; the active encoder may be NVENC or Media
Foundation as long as the replacement encoder can recreate the same
media contract. Exact CDS geometry alone advertises false and the
`display_resolution.resize` capability reports why, so the client need not
attempt an unsupported resize. A valid, newer `display_update` retires and
drains the old encoded generation, retargets the same journaled output,
revalidates the configured adapter/output binding, and starts a pinned
replacement generation with a fresh IDR. The accepted result reports the applied
size only after replacement capenc READY. NVAPI retargeting refreshes the driver
configuration after cleaning the previous custom timing; if its topology commit
rejects that fresh configuration, the already-checkpointed timing is applied
through the Windows display mode API. Any failed retarget or replacement-media
start reapplies the previous exact mode and restarts its media generation before
rejecting the update, so the QUIC attachment and broker-owned resume/admission
authority remain live. Invalid or stale requests are rejected without changing
the stream. Negotiated MF leases advertise false because they cannot
preserve the same exact-output contract during a live retarget.

Shared plumbing: `session.rs::session_display_plan` consumes `AuthResponse.displays_mode`
+ the full monitor list (`""`/`single_primary`/`windowed`/`match_layout` all plan one
mirrored display today; multi-monitor `match_layout` degrades loudly to the client primary
monitor and reports the degradation in `display_resolution` — the
`SessionDisplayPlan { monitors: Vec<MonitorPlan> }` seam is where Milestone 2 loops).
Unknown mode tokens and monitor-count safety failures still reach the Deck as `auth_result`
messages before authentication. Restore is
unchanged mechanics: the pre-mutation full `QueryDisplayConfig` snapshot re-activates
disabled paths and the original primary on lease drop, explicit restore, and the crash
watchdog (journal version 4); restore verification also proves the output count
returned to the original, and NVAPI-stage failures cannot strand the journal (see the
lesson above). `stream_session` refuses to bind the Microsoft Basic adapter for NVENC
sessions and requires the resolved output rect to equal the session rect.

**Live evidence (2026-07-17):** pier-windows.example.internal (GRID V100D, NVENC/`ExactIsolated`) — exact
1800x1168 mirror, single primary output, reconnect round-trip after the journal fix;
development workstation (VMware SVGA, MF/`NegotiatedMacroblock16`) — 16-aligned 1792x1168
stream with correct color and the idle keepalive cadence visibly dropping encode fps
on a static desktop.

**Proxmox CPU-only evidence (2026-07-24):** SPICE/QXL-compatible PCI display
(`VEN_1B36:DEV_0100`) exposed through `Microsoft Basic Render Driver`, D3D11 feature
level 11.0, no D3D11 video device, and the inbox MF H.264 MFT. WGC + MF produced
canonical READY and a stable 1280x800/30 stream after the 1792x1168 custom mode was
rejected. Adding SPICE-backed ICH9 HDA created the default render endpoint: WASAPI
then ran 48 kHz stereo with zero host queue drops, capture errors, or restarts in the
observed successful session. Deck reported no playback underruns. One startup
discontinuity/underrun, one Deck audio-inbox drop, and bounded latency trims were
observed without audible degradation. Without the emulated audio device, video
continued but WASAPI correctly retried because no default console render endpoint
existed.

## Deferred / roadmap

- Obtain the production Authenticode certificate; the deployed MSVC lab artifacts are unsigned.
- Live-test traditional AD credentials on an AD-joined host (serialization is covered).
- Record production-signed Credential Provider acceptance; lab binaries remain
  unsigned development artifacts.
- Wire the EDID-purge restore fallback if not already the default.
- Native Event Log sink readback and fleet-rule validation on a real host
  remain pending platform-owner operational review (pure formatters, fake
  backends, and bounded query tests cover this PR; see "Lifecycle Event Log").
- Forward the live session correlation id into the crash watchdog's command
  line instead of a freshly generated one, if a low-risk seam for that
  appears in a follow-on display-recovery change.

## Done since 2026-07-15

- **Distinct Arcen Credential Provider CLSID** — regenerated to
  `{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}`; the shipping product no longer collides with
  other remote-desktop software on the same fleet.
- **OpenH264 software encoder** — the Windows Pier compiles `software-h264`
  alongside `nvenc`. `--encoder auto` (the default) probes the NVIDIA encode API and falls
  back to source-built OpenH264 on hosts without NVIDIA (VMware SVGA, Intel-only, etc.). Live path
  in `hosts/capenc/src/win_mf.rs` uses WGC exclusively (DDA on VMware SVGA is unreliable).
- **VMware SVGA live stream** — `development workstation` proved console-only first-party
  target binding, VMware Tools display negotiation, exact-output WGC capture,
  MF H.264 at 30 fps, mouse/keyboard, audio, and verified restore. Explicit MF
  aligns the desktop to complete 16×16 macroblocks (1800×1168 → 1792×1168)
  instead of cropping encoded pixels. RDP protocol sessions are rejected before
  display mutation. The VMware console viewer mirrors the same Windows session
  and must keep Autofit disabled during Branch-A testing.
- **GPU diagnostics** — `arcen-pier diagnose-host [--json]` inventories adapter
  identity/type, D3D11/video capability, outputs/topology, installed encoder
  runtimes, and a same-adapter recommendation without mutating the display.
- **On-host TLS cert helper** — `hosts/windows/scripts/new-host-cert.ps1`
  generate-if-missing creates a P-256 self-signed server certificate with
  hostname/FQDN/non-loopback IP SANs, exports PEM cert + PKCS#8 key into
  `%ProgramData%\Arcen\tls\` with SYSTEM/Administrators-only ACLs, and writes
  whole-certificate and SPKI SHA-256 pins. Same-key renewal and trust-changing
  rekey are separate explicit commands.
- **Exact per-encoder display mirroring** — `DisplayPolicy::ExactIsolated` (NVENC) /
  `Negotiated` (MF/VMware), `SessionDisplayPlan` consuming `displays_mode` + monitors,
  honest ServerHello layout, journal-stranding restore fix. See the mirroring section.
- **MF hot-path perf** — fused single-pass BGRA→NV12 (each source byte read once,
  vectorizable), no per-frame plane clone, 1 s idle keepalive cadence on static desktops,
  cached MFT output-buffer size, reused drain scratch.

## Resume pointer

- **Status (2026-07-17):** ✅ ALL WINDOWS MILESTONES MERGED TO `main` (PRs #18/#19/#20) and
  live-confirmed on both lab hosts. `pier-windows.example.internal` (`203.0.113.11`, GRID V100D):
  cold-boot login + NVENC `ExactIsolated` mirroring + reconnect round-trip.
  `development workstation` (`203.0.113.12`, VMware SVGA): MF `Negotiated` streaming with idle
  cadence. `ArcenPier` runs as an automatic LocalSystem SCM service on
  `:18444/udp` on both;
  deployed binaries match `main`; the repo carries a single branch. Note: pier-windows.example.internal's
  original native console mode is lost (journal history began from a mutated state) — the
  console parks at 1800x1168 until the accept-current-baseline command exists.
- **Next Windows work:** multi-monitor Match-My-Layout (Milestone 2: per-head arming,
  per-monitor capenc, `VideoHeader.monitor_id`, Deck multi-stream rendering — seams in
  place), production Authenticode signing, the parked display items in
  [`todo_later.md`](todo_later.md) (standard-user path, EDID-purge restore,
  accept-current-baseline), and MF dirty-block skip / adaptive fps.
- **Cert note:** each Windows Pier needs a TLS certificate whose SAN covers
  every configured connection name; enterprise PEM is administrator-managed,
  while `scripts/new-host-cert.ps1` is the absent-only SMB bootstrap.
