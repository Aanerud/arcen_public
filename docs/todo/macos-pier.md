# macOS Pier: implementation and delivery plan

**Status:** Revised parity plan, 2026-09-11; updated with implementation
evidence on 2026-09-13. This remains a plan for the missing release contract:
the logged-in Aqua host path now exists, but no supported macOS Pier installer
or new permission grant is delivered by this document.

**Target confirmed by the repository owner:** Apple silicon, macOS 26 and later.
Dedicated hosts may have FileVault disabled; Arcen must support remote sign-in
from the macOS login window after a cold boot. Qualify each supported macOS
release rather than assuming future OS versions preserve every capability.

**Review:** Shared/Architecture owns the shared APIs and product boundary.
Release/Security must approve authentication, privileged IPC, entitlements,
installer transactions, dependencies, and release claims. Establish macOS Host
ownership before adding the product to workspace members. This plan does not
itself change the active scope in [ADR 0004](../adr/0004-platform-scope.md).

## 1. The delivery contract

### One host contract across all operating systems

Arcen does not ship three different kinds of Pier. Linux, Windows, and macOS
must implement the same host contract:

- one shared authentication, admission, session-generation, reconnect,
  display-topology, media-plan, input, clipboard, peripheral, telemetry, and
  recovery model;
- one common `pier.json` schema and one common certificate/trust lifecycle;
- one common install, upgrade, rollback, uninstall, support-bundle, and
  diagnostic contract;
- one feature/capability vocabulary and one truthful failure/degradation model;
- one acceptance matrix covering every required feature on every supported OS.

Only the irreducible adapter is platform-owned: service registration,
credential/session activation, display creation, capture, encoding, input
injection, pasteboard/clipboard access, audio devices, USB host control,
filesystem/ACL primitives, and code-signing/entitlement plumbing.

An OS may report a capability as unavailable during development, but that is a
failed parity gate, not permission to redefine the product. Existing Linux-only
microphone and native-tablet behavior, and the current Windows native-tablet
failure, therefore remain work items before a complete host release. A
platform-specific implementation must not add a second policy, protocol,
installer lifecycle, or recovery state machine.

The parity target is behavioral rather than identical binaries: the same
request must produce the same result, explicit refusal, or documented
degradation class, with only the native evidence and reason differing.

Deliver a Developer ID signed, notarized, stapled `.pkg` containing the complete
supported macOS Pier. The normal application and network service do not run as
root. Initial installation includes the macOS approvals actually required by
the signed components; subsequent boots and sessions must not require someone
at the Mac to start a program or log in first.

The release acceptance path is:

1. Install on a clean, supported dedicated Mac and complete explicit onboarding.
2. Shut down and cold-start the machine, with automatic login disabled and
   FileVault off for this acceptance profile.
3. Connect from the existing Deck over QUIC/TLS 1.3 on UDP 18444, accept any
   configured login banner, and authenticate as a permitted macOS account.
4. Enter that account's real physical-console desktop, with the negotiated
   media, input, peripheral, and policy features working.
5. Survive disconnect, reconnect, lock, logout, service failure, and another
   boot without losing local login or leaving display/input changes behind.
6. Upgrade, roll back, and uninstall without damaging normal macOS operation.

An installer that merely places files successfully is not enough. Neither is a
logged-in-user screen-sharing demo. Intermediate packages are development
milestones, not completion of this contract.

The generic shared lifecycle API must not provide a readiness bypass:
`NativeSessionReady` and `StreamReady` are accepted only through
evidence-gated operations that prove the current session, outputs,
capture/encoder, and input cleanup authority.

FileVault remains the operator's decision: **the installer must not disable
it**. If it is enabled, report the different boot limitation explicitly.
[Apple documents SSH FileVault unlock on Apple silicon/macOS 26][A1], but that
is not an Arcen pre-unlock mechanism. Do not introduce SSH, VNC, RDP, WSS, or
Apple Screen Sharing as a hidden fallback. Do not disable SIP or require
Reduced Security to make the default installation work.

## 2. What the investigation establishes

| Evidence | Consequence for this plan |
| --- | --- |
| [The current feature matrix](../../README.md#what-a-session-carries) includes capabilities that are not symmetric between Linux and Windows. Microphone ingress and Native tablet USB are Linux-only today, and Windows currently rejects Native tablet mode. | Treat these as parity defects to resolve or explicitly block the release; do not quietly redefine the common host contract around the weakest implementation. |
| The Deck's [accepted helper design](../adr/0011-macos-privileged-usb-helper.md) separates a small privileged executable from the GUI. Its implemented transport is a UID-authenticated Unix socket; code-signing-pinned XPC is still a later tranche. | Reuse the privilege separation, not an assumption that the final XPC boundary already exists. Start the new Pier's privileged boundary with application-identity authentication. |
| The Deck helper is behind `usb-hard-lab`, and captures a physical USB device for export. | It is neither a production-ready Mac USB importer nor a substitute for the host-side work below. |
| The supplied third-party deployment metadata has a service account, separate system services, an Aqua/LoginWindow agent, an authorization plug-in, an audio HAL plug-in, and a recovery component. | These are useful architectural observations. They do not establish which capture APIs, login mechanisms, or failure guarantees that product uses. |
| Apple still publishes [AuthorizationPluginInterface][A4], and the installed macOS 26.5 SDK declares `AuthorizationPluginCreate`. | An original, minimal authorization plug-in is a concrete investigation path. API existence is not proof of unattended console login or permission to bypass native authentication. |
| The macOS 26.5 ScreenCaptureKit SDK describes `RGhA` FP16 and `xf44` 10-bit 4:4:4 output, and limits HDR capture to Apple silicon. | There is a native high-precision capture path to investigate. A wide output format alone does not prove genuine source precision. |
| A hardware-required VideoToolbox property query on the development Mac advertised HEVC `Main44410`, as well as Main/Main10/Main42210. The equivalent AV1 query returned `-12908`. | Do not claim that Mac encoding must be 4:2:0, or infer encoding support from Deck decoding. These are advertised properties on one Mac, not real-frame, hardware-use, concurrency, or throughput proof. |
| The SDK declaration for [IOUSBHostControllerInterface][A7] explicitly supports remote or synthetic USB devices and requires `com.apple.developer.usb.host-controller-interface`. | A native entitled USB importer is a real candidate. This is a different boundary from seizing a physical USB device or writing a USB device driver. Root is not assumed to replace the entitlement. |
| A cleared, supported native virtual-display creation route and a complete deskside-privacy route have not been established here. | These need explicit feasibility gates. Do not invent a DisplayDriverKit API, silently use an undocumented API, or count a hardware dongle as a software-only solution. |

Apple developer credentials and access to required entitlements are assumed
available as requested. No signing identities, private keys, or provisioning
profiles were inspected. The exact entitlement on each final executable must
still be verified during signed qualification.

### Reference-material boundary

The investigation used deployment metadata and installer behavior as prior art.
It did not execute or disassemble the supplied binaries. No vendor source,
scripts, declarations, resources, plug-ins, or binaries belong in the
implementation, tests, package, or committed design evidence.

Implement from Arcen's contracts and Apple's supported interfaces, with original
tests. Rewriting an implementation in Rust does not by itself make it original
or establish redistribution rights. Preserve [ORIGINS](../../legal/ORIGINS.md)
and the exhaustive [third-party notices](../../legal/THIRD_PARTY_NOTICES.md);
any new dependency or nonpublic API route requires the relevant review.

## 3. Shared first, without creating another platform monolith

Do not start by copying `hosts/windows/src/session.rs` or the Linux Pier into
`hosts/macos/`. The Windows session file currently exceeds 12,000 lines.
Conversely, the two 199-line session-admission modules differ principally in
visibility and are an immediate candidate for a shared extraction.

Keep domain decisions in the existing crates:

| Shared owner | Reuse and extend here |
| --- | --- |
| `arcen-protocol` | Existing handshake and frame shapes; bounded helper messages; additive capability, readiness, and failure vocabulary where necessary. Preserve old-peer behavior through negotiated extensions. |
| `arcen-transport` | Certificate lifecycle, pinning, TLS policy, QUIC endpoint/framing helpers, and bounded transport behavior. Preserve the dependency-light default and opt-in `quic`. |
| `arcen-session` | Capacity-one admission, console ownership, permission/readiness decisions, login/session transitions, reconnect holds, restore/recovery decisions, and `PierConfig<MacOsPlatformConfig>`. |
| `arcen-identity` | Existing disclaimer evidence and resume grants; bounded, single-use local authorization evidence where needed. Native account authentication stays in the OS adapter. |
| `arcen-media` | Complete presets, source colour descriptions, conversion/reference maths, codec admission, encoded access-unit normalization, audio processing, and clipboard policy. |
| `arcen-input` | Ordering, region transforms, pressed-state cleanup, pointer/cursor authority, and typed pen semantics. |
| `arcen-outputs` | Existing `OutputProvider`, capability checks, atomic multi-output startup, topology, resource admission, and rollback transactions. Do not introduce a competing Mac display lifecycle. |
| `arcen-usb-bridge` | Existing allowed profiles, descriptor validation, attachment state, URB accounting, cancellation, and recovery policy. |
| `arcen-keel` | Damage tracking where the selected capture/conversion path actually benefits from it. |
| `arcen-telemetry` / `arcen-observability` | Common event names, typed failures, correlation, redaction, bounded logging, and support evidence. |

**Proposed additional crate, subject to Shared/Architecture review:**
`shared/pier/` / `arcen-pier-core`, for reusable cross-domain host orchestration.
It coordinates those domain crates; it does not duplicate their policy.
Keep its state/action core OS-free and deterministic. Put any reusable async
driver behind an explicit runtime feature so small helpers do not acquire
Tokio, Quinn, or media dependencies accidentally.

The reason for this crate is to avoid a third copy of handshake-to-session and
session-to-media coordination, not to relocate the entire Windows monolith.
Extract and adopt bounded slices on existing Piers with characterization tests
before making the Mac the third consumer. Preserve platform-specific native
operations and behavior throughout the migration.

Native ports return typed evidence and accept bounded operations: account
authentication, console-session observation/activation, capture, encoding,
input, audio, clipboard, and privileged mutations. Reuse `OutputProvider`
directly. No raw Objective-C object, Mach port, or other native handle crosses a
shared public API; the adapter owns native resources behind scoped identities.

Products must not depend on `clients/macos` to reuse its helper or VideoToolbox
code, including through source-path inclusion. Move portable helper lifecycle
decisions into shared crates. Keep the small ServiceManagement/FFI calls native.
A common macOS-only support crate is justified only by demonstrated native
reuse and an approved platform-owned location; do not put OS calls in `shared/`.

## 4. Native process and privilege boundary

Proposed layout: `hosts/macos/`, with local ownership guidance, plus Pier
packaging alongside the existing Deck packaging under `packaging/macos/`.
All names below are proposed components, not existing artifacts.

```text
Deck -- QUIC/TLS 1.3, UDP 18444 --> arcen-pier
                                      |
                             authenticated local IPC
                               /              \
                arcen-pier-privileged      arcen-pier-agent
                    narrow root broker    LoginWindow / Aqua context
                                                |
                                      ScreenCaptureKit, VideoToolbox,
                                      CoreGraphics, CoreAudio, AppKit
```

| Component | Execution context and responsibility |
| --- | --- |
| `arcen-pier` | System-started service under a dedicated non-login account. Owns the network connection and shared orchestration, not root authority or a GUI session. UDP 18444 does not require root. |
| `arcen-pier-privileged` | Small system-started broker. Only operations proven to require privilege: native authentication/session integration, approved machine-scoped mutations, and recovery. No QUIC parser, renderer, video encoder, or settings UI. |
| `arcen-pier-agent` | Native session adapter, launched in the required LoginWindow or Aqua bootstrap context. Captures, encodes, injects permitted input, and handles session audio/clipboard. Do not run the Aqua agent as root. |
| `arcen-pier-setup` | Ordinary-user onboarding, service registration/status, diagnostics, and approval guidance. It is not the boot-time host runtime. |
| `arcen-pier-auth` | Minimal authorization plug-in only if the native-login proof requires it. Runs in the authorization engine's prescribed context, with no transport/media stack. |
| `arcen-pier-audio` | Minimal AudioServerPlugIn adapter for remote microphone ingress; runs in the OS audio service's context. Audio transport and processing do not run in its real-time callback. |
| `arcen-pier-usb` | Isolated, appropriately entitled user-mode USB host-controller adapter. Use root only if the qualified OS interface additionally requires it. It is not the Deck's USB exporter. |

A single package may contain several small executables. Do not fuse the GUI,
network, or codec stack into a root executable to imitate Linux's multicall
packaging. Process separation is part of the Mac privilege boundary.

### Local IPC and resource ownership

- Authenticate both the service and caller using supported XPC code-signing
  requirements and audit-token validation. Bind authority to the exact role,
  principal, native session, and generation; a matching Team ID or UID alone is
  insufficient. Do not authenticate by a PID lookup vulnerable to reuse.
- Define typed, bounded messages in `arcen-protocol`; no arbitrary command,
  file-path, environment, account-switch, or shell-execution RPC.
- The broker validates native evidence independently. An unprivileged caller
  cannot turn a claimed permission or user identity into root authority.
- Credentials never enter process arguments, environment variables, plists,
  logs, journals, or generic session state. Any handoff is authenticated,
  short-lived, one-use, bounded, and cleared when consumed or cancelled.
- Keep native objects on their required run loop/queue. Use bounded queues and
  explicit cancellation between native callbacks and the shared runtime.
  Do not block capture/audio callbacks on network writes.
- Rust wrappers own CoreFoundation/Objective-C lifetimes, callback contexts,
  streams, encoders, and device handles. Document FFI invariants and prevent
  unwinding across OS callbacks. Drain/invalidate callbacks before freeing
  their backing state; do not add unchecked `Send`/`Sync` implementations.
- Use explicit teardown plus crash-safe recovery. `Drop` cannot restore
  machine state after a process crash or power loss.

## 5. Cold boot, native login, and session transitions

Model service readiness separately from graphical-session readiness:

```text
NotReady -> Listening -> NativeLoginPending -> SessionReady -> Streaming
                           |                       |
                     NativeLoginFailed       Locked / Reconnecting
                                                   |
                                            LoggedOut / Recovering
```

The shared model needs distinct evidence for boot identity, native console
identity, authenticated principal, session generation, permissions, output
binding, and encoder readiness. A password check, running launchd job, or open
UDP socket must not manufacture a `SessionReady` result.

The preferred remote sign-in flow preserves the current banner/authentication
ordering. After network authentication and native login coordination, wait for
the correct user's real Aqua session, attach its agent, establish outputs and
media, and only then advertise a usable session.

**Cold-boot sign-in need not require streaming unauthenticated loginwindow
pixels.** First prove credential-mediated native console login. If a separate
loginwindow capture/input path is necessary for the required login or unlock
workflow, prove that path separately; do not assume ScreenCaptureKit in a root
daemon can provide it.

Investigate an original Authorization Services plug-in and authenticated broker
handoff without replacing native account/password verification. A successful
Directory Services credential check alone does not create or unlock a desktop.
Preserve Apple's normal local login mechanisms. Any authorization-database
installation must be narrowly scoped, journalled, reversible, and qualified
against failed upgrades and a missing/broken plug-in.

The LoginWindow agent and Aqua agent are different session contexts, even when
they use the same executable. Fence all handoffs with session generations.
Logout, fast user switching, or a new boot invalidates stale capture handles,
input state, pending credential handoffs, and inappropriate resume authority.
Never attach a dropped connection to another user's desktop.

Retain capacity-one admission and the bounded reconnect reservation. Lock
versus logout versus service restart must follow explicit shared policy, not
platform-local booleans. The recovery component must restore local usability
even when the network service or session agent is no longer running.

## 6. Separate media contracts

Use native capture and encode adapters under `hosts/macos/`; do not widen the
Linux NvFBC or Windows eight-bit fast paths. ScreenCaptureKit configuration
objects may share native API plumbing, but the resolved pipelines remain
separate complete contracts.

| Preset | Proposed Mac source-to-Deck pipeline |
| --- | --- |
| Auto | ScreenCaptureKit SDR, proven 8-bit surface -> IOSurface/CoreVideo input and any required native format conversion -> VideoToolbox H.264/HEVC 4:2:0 8-bit at 30 fps -> existing ordinary SDR Deck path. |
| Speed | The independent eight-bit fast-path configuration above at 60 fps. No FP16 conversion or CPU readback introduced to support fidelity modes. |
| Grading | ScreenCaptureKit with proven high-precision SDR source, initially investigate `RGhA` FP16 or verified matching `xf44` -> shared source-specific SDR transfer/matrix/range contract -> 10-bit 4:4:4 CoreVideo surface -> proven VideoToolbox HEVC 4:4:4 10-bit, full-range BT.709 at 30 fps -> Deck native `xf44`/10-bit Metal, EDR off. |
| HDR | ScreenCaptureKit canonical-display HDR capture from the exact bound target -> verified source transfer, primaries, luminance metadata and precision -> shared conversion/validation to full-range BT.2020/PQ -> 10-bit 4:4:4 CoreVideo surface -> proven HEVC 4:4:4 10-bit at 30 fps -> the Deck's PQ/EDR presentation. |

For remote HDR, investigate canonical-display output, not a local-display
optimization whose interpretation depends on the capture display. Query actual
sample metadata; do not assume the canonical preset produces PQ rather than
HLG or another representation.

The shared source description must carry transfer, primaries, matrix, range,
precision, and the luminance normalization needed by the conversion. **Do not
reuse Windows' fixed 80-nit scRGB assumption for an Apple FP16 buffer.**
If ScreenCaptureKit already supplies the exact target representation, validate
and preserve it instead of doing a redundant conversion.

Use shared CPU/reference conversion and test vectors as the initial correctness
oracle. If native Metal conversion is needed for the measured budget, keep GPU
API/kernel integration native and validate its output against that same shared
contract. Record any CPU mapping, conversion, allocation, upload, or surface
copy. An IOSurface handle alone is not proof of zero-copy capture.

Require real encoded frames, hardware-use evidence, parsed bitstream
depth/chroma/colour, precision ramps, and multi-encoder throughput before
advertising a fidelity mode. A profile list or accepted property does not prove
that a frame was encoded as requested. A 10-bit container made from an eight-bit
source fails Grading; HEVC Main10 4:2:0 does not satisfy Grading's 4:4:4 contract.

Normalize VideoToolbox length-prefixed samples and parameter sets through shared
access-unit/framing logic. Preserve the current wire format, keyframe/recovery
semantics, timestamps, per-monitor identity, and READY/hello truth.

Keep source-built OpenH264 as the explicitly allowed 8-bit 4:2:0 software floor.
It cannot rescue 4:4:4 fidelity requests. Do not advertise hardware AV1 from
Apple silicon model names or Deck decode support. Every degradation is explicit
in the shared plan and Deck UI; a degraded preview is not full-fidelity parity.

## 7. Feature work and parity evidence

| Feature | Mac adapter and required proof |
| --- | --- |
| Transport and trust | Reuse the direct QUIC carrier, TLS lifecycle, certificates/pins and limits. Keep the existing single bidirectional stream and framing; a new multi-stream/datagram transport is not part of this port. |
| Banner, native authentication, resume | Existing shared evidence/grants plus native login integration. Test banner-before-credentials, wrong credentials, disabled accounts, lock/logout, different console owners, expiry, and replay. |
| One to four monitors | Implement `OutputProvider` over native display inventory/configuration. Admit the whole real encoder set using shared quality/resource thresholds; never invent NVENC session counts for Apple hardware. |
| Headless and virtual outputs | Prove the actual no-monitor case and a supported creation/configuration route for required additional outputs, including HDR. Do not count a pre-existing physical display or an unshipped external driver as a software-created display. |
| Pointer, keyboard and cursor | Native CoreGraphics/event or approved HID adapter; shared ordering/transforms and cursor authority. Verify absolute/relative motion, scroll, layouts, modifiers, secure contexts, disconnect cleanup, and no doubled cursor. |
| Typed pen/tablet | Map shared pressure, tilt, rotation, eraser, proximity and buttons to native tablet events. Prove behavior in real receiving applications; use an approved virtual-HID path only if required. |
| Native tablet / Hard USB | Original `IOUSBHostControllerInterface` importer translating the existing approved profiles and supported URB operations. Prove native driver enumeration, reports, cancellation, stalls, unplug, reconnect, and restoration. Serialize controller state-machine callbacks as required by the SDK. |
| USB release prerequisites | Verify the exact host-controller entitlement and any native driver prerequisite. The Deck exporter is a separate prerequisite; its lab-only packaging and incomplete application-identity IPC cannot silently become production guarantees. Do not expand to arbitrary USB classes, cameras, or unsupported transfer types as part of parity. |
| Audio out | ScreenCaptureKit/CoreAudio native capture -> existing shared 48 kHz stereo Opus/PCM contracts. Prove clock behavior, silence, device/session changes, and absence of feedback. No HAL loopback driver unless the selected native capture path actually needs it. |
| Microphone in | Existing Deck consent and shared audio path -> original Core Audio HAL virtual input device. This is Linux parity, not an already-shipped Windows feature. No transport, blocking IPC, allocation, or file I/O in the real-time device callback. |
| Microphone consent | Preserve per-launch client opt-in and independent host enablement. A host virtual input device is not permission to capture a local microphone; denial/revocation produces explicit state, not silent success. |
| Clipboard | Native pasteboard adapter for the current text/image formats, with shared size limits, conversion, sequencing, echo suppression and direction policy. Never expose one user's clipboard to another. |
| Timezone | Preserve shared redirection and restore leases. Establish whether the qualified operation is session-local or machine-global. Do not silently replace Linux's session behavior with an unrestricted global Mac timezone mutation. A global route requires explicit host policy and scoped recovery. |
| Deskside privacy | Prove physical-screen blanking while remote capture remains useful, together with local keyboard/pointer suppression. Shared all-or-nothing admission, independent recovery, and local usability after crashes are mandatory. A black capture or input-only suppression does not pass. |
| Session failure/reconnection | Fresh capture/encoder attachment as necessary, current generation, fresh keyframe, discarded stale queues, pressed-input release, and correct reconnect reservation. Test sleep/wake and network changes; do not equate sleeping with remotely reachable. |
| Administration/support | Common `pier.json` schema with native fields under `platform`; local diagnostic/status commands, live settings where already supported, redacted bounded logs and support bundles. Keep machine identity/configuration separate from app replacement. |

Capability absence must be explicit and fail closed where policy requires it.
There is no claim of complete parity until the required rows pass or the owner
explicitly changes the product scope. Existing Linux/Windows asymmetries are
not blanket permission to omit a difficult Mac feature.

## 8. Package, permissions, and lifecycle

Prefer a `.pkg` over a self-extracting root copy of the full Pier. The package
is opened with Installer.app on the Mac it installs, because its privacy
permissions can only be approved there. Its installation check refuses the
command-line `installer` unless an administrator opts in with
`ARCEN_ALLOW_COMMAND_LINE_INSTALL=1`, for example on Macs that get the
permissions from an MDM profile. See `hosts/macos/PACKAGING.md`.

Package an application bundle with the setup tool, native agents, broker, and
service definitions. Keep machine configuration, TLS material, recovery
journals, and logs in separately owned locations. Give the network account only
the material it needs; never solve access failures with world-writable files or
by running the entire bundle as root.

Use the existing SMAppService direction for bundled service registration,
including approval/status handling, and prove restart-before-login behavior.
Give each job exactly one installation/lifecycle owner: do not both register a
bundled SMAppService job and install a second legacy launchd job for the same
role. Qualify the machine-wide Installer/onboarding flow explicitly rather
than assuming the package's administrator authorization grants every service
or privacy approval.

The onboarding and diagnostic model should distinguish:

- installed files from approved and live background services;
- the identity of the actual capture/input executable from that of the setup
  app, including its effective signed entitlements;
- initial Screen Recording/Accessibility authorization, persistent capture
  behavior, and later denial/revocation;
- an approved entitlement from a granted TCC permission and a correct GUI
  bootstrap context;
- a running network service from a native session capable of the requested
  capture, input, display and peripheral contract.

The [persistent-content-capture entitlement][A6] is a specific Apple capability,
not a general privacy bypass. Verify its approved use for Arcen and its actual
behavior on the signed session agent. Do not promise a particular number of
prompts before this is measured.

For managed deployment, [PPPC][A8] can preconfigure some permissions, including
Accessibility. Its ScreenCapture service does not itself silently grant screen
access. The identity payload supports
`AllowStandardUserToSetSystemService` for permitted services. Keep an unmanaged
onboarding path as well; do not require MDM without an explicit scope decision.
Track policy/API changes when qualifying later macOS releases.

For installation, upgrade, rollback, and uninstall:

1. Preflight supported OS/hardware, signing, target paths, conflicts, FileVault
   boot profile, required capabilities, and available local recovery.
2. Verify every payload before mutation. Install transactionally; keep stable
   code-signing identities and preserve machine configuration and TLS identity.
3. Register services and guide required approvals in the correct user context.
   A denied or pending approval is a visible incomplete setup state.
4. Verify the effective running components and complete an authenticated
   end-to-end session before declaring the deployment healthy.
5. Retain rollback material until the new version is proven. Journal changes
   outside the bundle, especially authorization mechanisms and audio plug-ins.
6. Uninstall in dependency order, restoring display/input/timezone and removing
   only Arcen-owned registrations and plug-ins. Do not overwrite unrelated
   administrator changes. Stop recovery supervision last.

Sign nested components inside-out with appropriate hardened-runtime settings
and individual entitlements; sign the package with Developer ID Installer.
Notarize and staple the deliverables. Produce hashes, exact-source provenance,
and complete notices for everything shipped. Neither a copied Deck provisioning
profile nor successful outer-bundle signing proves the inner services are valid.

## 9. Cross-platform parity and installer work

The macOS package cannot be completed in isolation. The implementation must
first define and then adopt a shared host contract on Linux and Windows as
well.

### P1: common host core

Create the smallest reviewed `arcen-pier-core` surface in `shared/`:

- typed host lifecycle and session-generation state machine;
- admission, reconnect reservation, lock/logout/fast-user-switch transitions;
- native evidence requirements for authentication, console ownership,
  permissions, outputs, capture, encoder readiness and recovery;
- capability negotiation, explicit degradation/failure reasons and applied-plan
  truth;
- bounded local-helper IPC messages and correlation identifiers;
- common diagnostics, support-bundle inputs, lifecycle events and cleanup
  obligations.

Characterize Linux and Windows behavior first, then migrate both existing Piers
to the core before adding macOS-specific orchestration. No host may retain a
private copy of these rules after migration.

### P2: common configuration and certificate lifecycle

Make the common `pier.json` schema authoritative on all hosts. Platform
sections may select native paths and entitlements, but may not redefine
transport, logging, TLS policy, session limits, clipboard policy, display
semantics, or recovery rules.

Move certificate lifecycle decisions into shared policy and keep only the
platform file/ACL/keychain operations native. Every host must support:

1. first creation when no identity exists;
2. refusal on partial or foreign material;
3. renewal without changing the trusted key;
4. explicit adoption of eligible legacy material;
5. explicit trust-changing rekey;
6. fingerprints/ownership markers;
7. atomic staging, fsync/close, publication and interrupted-transaction
   recovery;
8. TLS 1.3 validation, SAN/EKU/key-usage checks and expiry diagnostics.

Linux, Windows, and macOS must expose the same operator concepts and failure
states even though Linux uses protected files, Windows uses ACL-protected
ProgramData, and macOS may use protected files plus Keychain/Installer
integration.

### P3: common installer lifecycle

Define a platform-neutral installer manifest and transaction journal describing
payloads, service roles, owned paths, certificates, permissions, registrations,
rollback state and uninstall ownership. Implement one lifecycle sequence on
each OS:

1. preflight OS/hardware/signing/ownership/conflict checks;
2. verify payload hashes and provenance;
3. stage binaries/configuration/certificates/helpers;
4. stop or quiesce the old service safely;
5. register only Arcen-owned services and permissions;
6. start the service and verify effective component identity;
7. run authenticated smoke validation before declaring success;
8. retain rollback material until the smoke proof passes;
9. on failure, restore the previous version and service state;
10. uninstall in dependency order and remove only Arcen-owned state.

The native operations differ (`systemd`, Windows Service/Credential Provider,
and launchd/ServiceManagement/Installer), but the transaction states, safety
rules, dry-run behavior, upgrade guarantees, certificate preservation, support
paths, and uninstall claims must be equivalent. A macOS `.pkg` is not complete
until it meets the same contract already documented for Linux and Windows.

### P4: parity implementation order

1. Extract/adopt the shared core on Linux and Windows with characterization
   tests.
2. Add the common installer/certificate manifest and conformance tests.
3. Close existing parity defects: Windows native tablet behavior, microphone
   capability exposure, and any other feature currently marked Linux-only.
4. Implement macOS native adapters behind the same ports.
5. Run the same request matrix against all three hosts, including unavailable,
   degraded, recovery and rollback cases.

### Parity stop conditions

Do not call the host implementation complete, publish a package, or describe
the three hosts as equivalent until:

- the same Deck request is accepted, degraded, or rejected through the same
  shared decision path on all hosts;
- every required feature has a native adapter or an approved release blocker;
- install, upgrade, rollback, certificate renewal/rekey, support-bundle,
  service recovery, and uninstall tests pass on all three operating systems;
- no platform host contains duplicate portable policy or a hidden fallback;
- release evidence names the exact native limitation when behavior cannot be
  identical (for example FileVault pre-boot remains outside Arcen's path).

## 10. Phased implementation with stop/go gates

| Phase | Deliverable | Exit condition |
| --- | --- | --- |
| P0: signed feasibility | Small original probes and a disposable internal package on dedicated test Macs; no broad host refactor. | The native login, privilege, media, output/privacy, and USB paths below have measured evidence and an approved implementation route. Failed proofs are reported, not hidden behind stub success. |
| P0: signed feasibility | Small original probes and disposable internal packages on dedicated test machines; no broad host refactor. | Native login, display/privacy, media, input, clipboard, audio, USB, signing and entitlement routes have measured evidence or explicit blockers. |
| P1: shared foundation | Characterization tests; reviewed `arcen-pier-core`; common config/certificate/installer manifest; typed native ports and helper messages; Linux/Windows adoption. | Existing Linux/Windows behavior is preserved through shared code; default dependency purity remains intact; no third policy copy exists. |
| P2: macOS boot-to-desktop slice | macOS product, network service, narrow broker, session agent and necessary original auth adapter behind the shared core. | Install -> cold boot -> Deck authentication -> correct real desktop -> SDR video/input -> disconnect/reconnect/logout. This is a preview, not parity completion. |
| P3: shared media/output/peripheral closure | All hosts use the same complete contracts for Auto/Speed/Grading/HDR, displays, audio, clipboard, input, typed pen and native USB. | Same request matrix and degradation vocabulary pass on Linux, Windows and macOS; actual frames and native devices meet declared contracts. |
| P4: common shipping lifecycle | Equivalent install, certificate, permission, service, support, upgrade, rollback and uninstall transactions on all hosts. | Clean/admin/standard-user and failure-path lifecycle matrix passes without manual repair or broad runtime elevation. |
| P5: release qualification | Reproducible candidate artifacts and retained cross-platform evidence. | The full delivery contract and acceptance matrix pass on the supported hardware/OS matrix; owners approve support claims. |

### P0 must answer these questions before the architecture is treated as proven

| Gate | Concrete proof |
| --- | --- |
| Native boot/login | With FileVault off and auto-login off, reach the system service after real power-on and complete native console login for an existing standard user. Repeat logout/login, lock/unlock and denied credentials. Preserve local login when the Arcen auth component is absent or fails. |
| Session context/permissions | Determine which LoginWindow and Aqua operations actually work for the signed agent, and which require a plug-in or broker. Verify first approval, denial, revocation and persistence through reboot/update, including a permitted standard user who has never approved Arcen locally. A required first local visit by every remote user is a limitation, not unattended first-login parity. |
| Fidelity encoding | Capture a source precision pattern and encode actual HEVC 4:4:4 10-bit frames through the proposed VideoToolbox route. Inspect the bitstream and decoded ramp, hardware-use flag and copy boundaries. Repeat for BT.709 SDR and source-proven BT.2020/PQ HDR. |
| Output/privacy | Demonstrate no-monitor operation, required additional outputs, exact-target HDR and reversible physical privacy. If only a nonpublic interface or external prerequisite works, stop for explicit review rather than quietly changing the package contract. |
| Native USB | Run a correctly entitled host-controller adapter with an original synthetic device, then the approved remote tablet profile through Arcen's URB contract. Confirm native enumeration and safe detach. Entitlement presence alone is not completion. |
| Microphone device | Expose a working virtual input device using original HAL code and synthetic audio first. Demonstrate bounded real-time behavior and removal/recovery without making unrelated audio unusable. |

Do not invent a calendar estimate before P0. Its output should include the
measured constraints, remaining Apple/DTS questions, dependency decisions and
per-phase implementation estimates. The full product need not wait for every
feature to be implemented before P2 starts, but unsupported routes must not be
presented as solved architectural assumptions.

## 10. Acceptance and regression evidence

**Shared tests:** scripted native-adapter fakes for every lifecycle transition;
invalid/stale/replayed session evidence; capacity-one and reconnect bounds;
permission loss; partial output startup and rollback; all native failure
results; frame/IPC limits; codec/colour truth; clipboard isolation; input
ordering and cleanup; USB cancellation; audio bounds. New shared logic must
compile and pass tests without Apple frameworks.

**Native/package tests:** use real Apple silicon machines, not only a VM or an
already-authorized development account. Include the minimum supported OS and
each additional claimed OS/hardware class, clean standard/admin users and first
remote logins without a previous local Arcen permission grant,
non-English account/display settings, physical and headless topologies, SDR/HDR
displays, and every shipped entitled component.

The proposed reliability campaign includes ten real cold boots and one hundred
login/logout/lock/reconnect transitions per qualified configuration, with no
stale-session attachment or loss of local recovery. These are proposed release
test counts, not reliability measurements already obtained.

Exercise process crashes, helper denial/revocation, missing plug-ins, malformed
IPC, same-user wrong-application callers, expired handoffs, network loss during
login, service restart, sleep/wake, display hotplug, and device removal. Check
that local input, display arrangement, clipboard ownership and any global
timezone mutation are restored appropriately.

Verify the actual preset targets: Auto 30 fps, Speed 60 fps, Grading/HDR 30 fps,
with their exact negotiated precision/chroma/colour at the admitted geometry.
Use the existing shared admission and `QosTargets` contracts; do not invent
weaker Mac-only thresholds or admit four encoders because one encoder worked.
Record capture/encode/presentation latency, queue pressure, memory growth and
audio continuity, not just an average frame rate.

For every extraction, run the affected shared tests and strict Clippy gate,
then the affected product on its own OS. A successful Mac build does not
validate Linux or Windows. Preserve the transport/media/output dependency
purity checks, and add an explicit check that the privileged broker and auth
plug-in do not link the network, UI or media runtime.

The release record must identify what was built and exercised on each OS.
Follow the repository's manual platform-build policy; this plan does not
silently add CI permissions or hosted-platform builds. Run publication hygiene
before proposing implementation or release changes.

## 11. Research sources and limits

Repository sources include [the feature/preset matrix](../../README.md),
[shared ownership](../../shared/AGENTS.md),
[the shared output-provider interface](../../shared/outputs/src/provider/mod.rs),
[the parameterized host configuration](../../shared/session/src/pier_config.rs),
[Linux admission](../../hosts/linux/src/session_admission.rs),
[Windows admission](../../hosts/windows/src/session_admission.rs),
[the macOS helper ADR](../adr/0011-macos-privileged-usb-helper.md),
[its actual helper implementation](../../clients/macos/usb-helper/src/main.rs),
[its registration adapter](../../clients/macos/src/usb_helper_install.rs),
[peripheral permission boundaries](../architecture/macos-peripheral-access.md),
and [the existing transport contract](../architecture/transport.md).

Apple sources used for the platform design:

| Source | What it establishes, and what it does not |
| --- | --- |
| [FileVault management][A1] | Documents the separate SSH unlock route; not third-party QUIC access before unlock. |
| [TN2083: Daemons and Agents][A2] | Explains bootstrap/session boundaries. It is archived and is not proof that a modern capture API works at loginwindow. |
| [SMAppService][A3] | Bundled helper registration and approval model; not automatic TCC approval. |
| [AuthorizationPluginInterface][A4] | Public plug-in interface. SDK 26.5 also declares its entry point; remote console login still needs an original signed proof. |
| [ScreenCaptureKit HDR presentation][A5] and the current SDK | Separate SDR/HDR configuration, canonical-display use and precision formats; not bit-exact capture or encoding results for Arcen. |
| [Persistent Content Capture][A6] | An Apple-approved persistent capture capability, not proof of arbitrary privilege or loginwindow access. |
| [IOUSBHostControllerInterface][A7] and SDK 26.5 `IOUSBHostControllerInterface.h` | User-mode remote/synthetic USB controller route and the SDK's specific entitlement requirement; no importer has been built or entitled in this task. |
| [PPPC services][A8] and [identity policy][A9] | Service-specific managed permissions and standard-user approval controls; not a blanket silent Screen Recording grant. |
| [AudioServerPlugInDriverInterface][A10] | Native HAL plug-in interface; not an implemented or real-time-qualified microphone device. |
| [DriverKit creation guide][A11] | Native driver families and deployment context; no virtual-display solution has been established by this investigation. |

The local VideoToolbox observation used
`VTCopySupportedPropertyDictionaryForEncoder` with hardware acceleration
required. It did not capture a screen, encode frames, install a helper, change
TCC, modify FileVault, or edit the authorization database. No macOS Pier or
installer was built, and no Linux/Windows product build was run for this
planning-only change.

[A1]: https://support.apple.com/guide/security/managing-filevault-sec8447f5049/web
[A2]: https://developer.apple.com/library/archive/technotes/tn2083/_index.html
[A3]: https://developer.apple.com/documentation/servicemanagement/smappservice
[A4]: https://developer.apple.com/documentation/security/authorizationplugininterface
[A5]: https://developer.apple.com/videos/play/wwdc2024/10088/
[A6]: https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.persistent-content-capture
[A7]: https://developer.apple.com/documentation/iousbhost/iousbhostcontrollerinterface
[A8]: https://developer.apple.com/documentation/devicemanagement/privacypreferencespolicycontrol/services-data.dictionary
[A9]: https://developer.apple.com/documentation/devicemanagement/privacypreferencespolicycontrol/services-data.dictionary/identity
[A10]: https://developer.apple.com/documentation/coreaudio/audioserverplugindriverinterface
[A11]: https://developer.apple.com/documentation/driverkit/creating-a-driver-using-the-driverkit-sdk
