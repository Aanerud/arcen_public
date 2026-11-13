# ADR 0011: macOS Privileged USB Helper

**Status:** Accepted 2026-08-20.

> **Status note (0.19.0):** [ADR 0019](0019-iousbhost-usb-backend.md) moves
> this helper's capture from libusb to `IOUSBHost.framework`. Since 0.19.0
> every release helper is built with the `iousbhost` feature and captures
> with `IOUSBHost` `DeviceCapture`; a release build refuses a helper that
> does not link `IOUSBHost.framework`. libusb is still linked and remains
> selectable with `--backend libusb`. Every helper follows its hello with one
> info frame (tag `0x06`, `backend=<name>`) so the Deck can log which backend
> captured the device; older Decks skip it. The privilege boundary below is
> unchanged.

## Context

Hard USB currently works only when the whole Arcen Deck app is launched as root.
That is a lab shortcut, not a shippable model.

The alternative entitlement does not apply: `com.apple.vm.device-access` is
reserved for Mac App Store hypervisor apps supporting systems before macOS 27,
and every operation it gates also admits a privileged process. A
Developer-ID-signed Deck therefore neither needs nor can obtain it. The
supported route is to split the privileged code into a separate helper tool,
run only that as root, and reach it over IPC (see Apple's
[Service Management](https://developer.apple.com/documentation/servicemanagement)
documentation for `SMAppService` daemons).

This ADR records the helper design: what runs as root and how it is reached.

## Decision

### 1. The helper is a separate minimal binary, not a fused multicall subcommand

`hosts/linux/AGENTS.md` requires the Linux helper to be a multicall subcommand of
the single fused `arcen-pier` binary. **The macOS helper deliberately does not
follow that convention.**

This is not a platform limitation. `man 5 launchd.plist` confirms `BundleProgram`
maps to the first argument of `execv(3)` while `ProgramArguments` maps to the
second, so a plist could name `Contents/MacOS/arcen-deck` with argv
`["arcen-deck", "usb-bridge-helper"]` and match Linux exactly. We are declining a
capability we demonstrably have.

The reason is that the two helpers sit in different places relative to privilege.
On Linux the helper is spawned by an already-privileged service, so fusion costs
nothing. On macOS the helper **is** the privilege boundary and runs as root.
Fusing it into `arcen-deck` would place the GUI, wgpu/Metal renderer, video
decoder, QUIC transport, and settings surface inside a root process image. The
helper must therefore link none of the client's UI, transport, or media code.

**Owner: Shared/Architecture.**

### 2. Blessing, IPC, and the caller-authentication boundary

- Registration via `SMAppService.daemon(plistName:)`. `SMJobBless` is deprecated
  and `AuthorizationExecuteWithPrivileges` is forbidden.
- IPC over an XPC Mach service, carrying the existing `arcen-protocol` URB frames
  so the macOS and Linux helper boundaries validate the same shapes under the
  same bounds.
- Every connection pinned with
  `NSXPCListener.setConnectionCodeSigningRequirement`, verified present at
  macOS 13+ in the SDK, with an audit-token and `SecCodeCopyGuestWithAttributes`
  check as defense in depth. Authentication by `xpc_connection_get_pid()` is
  forbidden, being the CVE-2019-8526 PID-reuse class.
- The helper independently re-runs `arcen_usb_bridge::evaluate_profile` against
  descriptors it read itself and never trusts a device claim from Deck.

**Owner: Release/Security.**

### 3. Signing changes

`packaging/macos/build-deck-app.sh` stops using `codesign --deep` and signs
inside-out: helper first with `--options runtime --timestamp`, then the main
binary, then the bundle. `packaging/macos/validate_release_inputs.py` must learn
about the nested helper so release validation cannot silently ignore it.

**Owner: Release/Security.**

### Delivery tranches

The decision above describes the shipped design. It arrives in two tranches,
because an XPC Mach service only exists once launchd publishes it — so the
transport cannot land before the daemon registration does.

| Tranche | Transport | Launch | Status |
| --- | --- | --- | --- |
| **1** | Root-owned Unix socket, peer authenticated with `getpeereid` | `sudo arcen-usb-helper` | **Implemented 2026-08-20** |
| **2** | Same socket | `SMAppService.daemon`, one approval | **Implemented 2026-08-20** — removes `sudo` |
| **3** | XPC Mach service pinned with `setConnectionCodeSigningRequirement` | unchanged | Not started |

Tranche 2 was verified on the development machine: `SMAppService` reports
`enabled`, BackgroundTaskManagement lists the daemon embedded under Arcen Deck
for team `<APPLE_TEAM_ID>`, and launchd runs it as root while Deck runs as the
ordinary console user. **No `sudo` is involved anywhere.**

Two things had to be right for registration to work at all, and neither is
obvious from the API:

- the helper binary must live in `Contents/MacOS/`, not `Contents/Resources/`;
- the daemon plist must carry `AssociatedBundleIdentifiers` naming the owning
  app.

And one trap cost real time: an unregistered daemon reports status **`notFound`**,
not `notRegistered`. The system log shows why — `smd` logs `Setting up
BundleProgram keys` successfully, then BackgroundTaskManagement answers
`record not found` and the status becomes 3. So `notFound` here means "never
registered", and code branching only on `notRegistered` silently never
registers. Equally, `register()` returning *"Operation not permitted"* on first
run is the documented normal path, not a failure: trust the resulting status.

Tranche 3 remains worthwhile because peer-uid proves *which user* is calling,
not *which program*; only a code-signing requirement makes the caller's
identity cryptographic.

Tranche 1 already delivers the property that matters most: Deck runs
unprivileged, so it no longer corrupts its own configuration ownership or loses
its graphics session, and the root surface drops from the whole 15 MB
application to a 521 KB binary with no async runtime, no transport stack, no UI
and no media code.

Tranches 1 and 2 deliberately deviate from the "XPC, not a Unix socket" rule,
and the deviation is bounded: it authenticates with `getpeereid` rather than the
peer pid — pid reuse being the specific race this must avoid — and the socket is
root-created, mode `0600`, owned by the single authorized uid. Peer-uid remains a weaker statement than a code-signing
requirement, which is what tranche 3 addresses.

*Amended 2026-11-09 (security fix).* The socket was `/var/run/arcen-usb-helper.sock`,
and the helper set its mode and owner with `chmod`/`chown` on that path after
`bind`. On macOS `/var/run` is `root:daemon` 0775 without the sticky bit, so a
member of group `daemon` could race a symbolic link onto the path and have the
root helper change the mode and owner of an arbitrary file. The Deck also never
checked who was listening. Now:

- The socket is `/private/var/db/arcen/run/usb-helper.sock`. Every ancestor is
  `root:wheel` 0755 and `run` is root's 0755, so only root can change anything
  on the path; the Deck can still reach the socket. The helper creates and
  verifies the directory at start from `/` down, opening each component with
  `O_DIRECTORY | O_NOFOLLOW` relative to its parent and checking owner and mode
  on the descriptor (`clients/macos/secure-dir`), and refuses to serve from any
  other directory.
- A stale entry is removed only if `fstatat(AT_SYMLINK_NOFOLLOW)` shows a
  socket. After `bind`, the mode (0600) and owner (the authorized uid) are set
  with `fchmodat`/`fchownat` and `AT_SYMLINK_NOFOLLOW` relative to the pinned
  directory descriptor, and checked again. The helper runs with umask 0077, set
  before any thread starts, so the socket is never connectable by others even
  before its mode is set.
- The Deck checks the helper with `getpeereid` right after connecting and
  refuses, with a logged reason, any peer that is not root.
- The helper's own `getpeereid` check of the client is unchanged.

`/private/var/db/arcen` survives a restart and is not removed when the Deck is
deleted; it holds only this empty `run` directory (and the IOUSBHost marker
directory, ADR 0019). After a Deck update, a helper that launchd started from
the previous version keeps listening on the old path until launchd restarts it
(restart the Mac, or turn the helper off and on in System Settings > General >
Login Items).

## Consequences

The user sees **one administrator prompt**, at helper registration, presented by
macOS through `SMAppService`. That is the intended and only escalation point.

It is worth being explicit about what that prompt is *not*, because the obvious
alternative is tempting and wrong. A prompt that escalates the **whole Deck app**
to root — the `AuthorizationExecuteWithPrivileges` shape — is deprecated since
macOS 10.7, performs no signature check on what it launches, and is precisely
the model Apple's guidance steers away from. The prompt in this design authorizes
*installing a small root daemon*, not running Deck as root. There is no
per-session password dialog: once approved, the daemon persists and Deck reaches
it over an authenticated XPC boundary.

### Measured cost of running the Deck as root

Running the whole app as root is not merely inelegant; it was measured breaking
things during a Hard USB attempt:

- `~/Library/Application Support/Arcen/settings.json` and `connections.json`
  were rewritten as `root:staff`, mode `0600`. A subsequent non-root Deck can no
  longer read or write its own configuration, and recovery needs a manual
  `chown`. Every root launch re-poisons them.
- The same session ran at ~6 fps with 72 CoreAudio underruns and a health state
  dominated by `fps`, against a host that normally streams far better — a
  root-launched GUI does not inherit the user's normal graphics and audio
  session context.

This is first-hand evidence that the helper is required rather than merely
preferred. `sudo` must never be documented as a user-facing workflow.

Deck must degrade to the typed Light path, not fail, when the helper is absent or
unapproved, and must route `SMAppService.Status.requiresApproval` to
`openSystemSettingsLoginItems()` rather than reporting Hard USB as broken.

Deleting the app removes the daemon registration.

On macOS 27 and later, Accessory Access is expected to invert this design rather
than delete it: Deck opens the accessory with user consent and hands it to a
helper via `createXPCRepresentation()`, at which point the helper no longer needs
to be root. Keeping the XPC boundary and URB contract identical now is what makes
that migration cheap. Whether `AAUSBAccessory.open()` actually pre-empts the
in-kernel HID driver for a HID-class tablet is not yet established.

## Alternatives rejected

| Alternative | Why rejected |
| --- | --- |
| Request `com.apple.vm.device-access` | Reserved for Mac App Store hypervisors |
| Run the whole Deck app as root | Breaks per-user state (above) and unacceptable for distribution |
| Prompt for admin at every launch and escalate the app | `AuthorizationExecuteWithPrivileges` is deprecated since 10.7 and validates nothing about the binary it runs |
| Fused multicall subcommand, matching Linux | Mechanically possible, but puts the GUI, renderer, decoder and transport in a root process image |
| Unix domain socket instead of XPC | Would require hand-rolling peer authentication, the one part that must not be hand-rolled |
| Wait for macOS 27 Accessory Access | Does not help any currently supported release, and its HID pre-emption behavior is still unconfirmed |
