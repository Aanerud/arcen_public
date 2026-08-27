# Trust Boundaries

Arcen treats clients, hosts, gateway peers, identity-provider responses,
network metadata, and persisted configuration as distinct trust inputs.

## Required separations

- OIDC establishes user identity; Entra is the first provider profile.
- OS-specific machine authentication establishes host identity.
- Session authorization binds verified user, machine, and policy decisions to a
  short-lived session.
- Transport encryption protects a channel but does not replace authorization.

Commercial Pier licensing was withdrawn when Arcen moved to AGPL-3.0 free
software; see [ADR 0006](../adr/0006-offline-pier-licensing.md). The remaining
capacity-one session gate is a physical host-resource boundary, not an
entitlement boundary.

## Direct-QUIC certificate boundary

Direct QUIC is the only live transport and listens on UDP 18444. The
dependency-light default of `arcen-transport` owns the common rustls/ring
posture and certificate/SPKI validation decisions. Product adapters own
protected PEM I/O and explicit reload; packaging helpers own optional SMB
issuance. The Pier process never invokes a helper, OpenSSL, a certificate
store, or a runtime issuer.

All PEM is untrusted until strict pre-listen validation succeeds. The leaf must
be current, non-CA, SAN-bearing, server-auth/digital-signature compatible, use
an admitted key, and match its private key. Configured DNS/IP identities are
exact SAN requirements. Windows enforces protected opened-handle/DACL rules;
Linux enforces `openat`/`O_NOFOLLOW`, ownership, size, regular-file, and mode
rules. Reload installs only a validated replacement and otherwise keeps the
last good still-valid key; expiry refuses new handshakes.

Deck system/private-CA modes remain chain and server-name based. Medium
security may capture only an otherwise-valid `UnknownIssuer` leaf, reject that
probe handshake, display actual hashes/validity and the peer address that
presented them, and accept an SPKI only after an explicit user action. The user
chooses between accepting for the current process and recording the identity in
`trusted_pins.json`; no Arcen protocol data precedes either. A recorded identity
is loaded before any connection and installs a pinning verifier rather than a
capturing one, so a subsequent mismatch is terminal and cannot be re-prompted.
Rekey requires new trust. Each concurrently dialled address captures into its
own slot, so a displayed fingerprint always belongs to the handshake that
produced it. The existing insecure Low mode remains separately double-gated, is
surfaced on screen while active, and is not a compatibility path for invalid
host material.

Secrets must not enter source, logs, workflow inputs visible to untrusted code,
or build artifacts. Security-sensitive changes require threat analysis, negative
tests, and Release/Security review.

## Microphone input boundary

Microphone input is disabled by default and requires independent operator policy,
Deck consent, backend availability, authenticated-session binding, and additive
microphone-v1 negotiation. It uses a distinct sequenced upstream frame and never
reinterprets host-to-client audio. Frames before authorization, after disable,
for another generation, SID/WTS session, or Linux UID are rejected. Disable and
teardown stop capture/publication, drain bounded queues, and zero reusable audio
storage. Payloads and device-private data are never logged or persisted.
Deck consent is launch-only and never reloaded as enabled. A session-lifetime
cancellation latch stops callback publication and outranks queued data and
socket writes on every exit path.

Linux creates and restores its source only in the authenticated non-root session
user's audio environment. Windows keeps native handle/IOCTL ownership host-side
and requires a tightly ACLed SYSTEM feeder plus WTS, binary SID, and monotonic
generation enforcement for producers and capture readers. Windows performs no
default-recording-role mutation: the user or recording application selects the
installed endpoint, and disable/disconnect only stop and zero Arcen-owned audio.
Unreapable Windows driver I/O is fatal to the owning session-agent process, so a
stale worker cannot survive into a new attachment. macOS permission and capture
failures fail closed. See
[`../architecture/audio-input.md`](../architecture/audio-input.md) and
[`../operations/audio-input.md`](../operations/audio-input.md). Windows remains
unavailable until the complete PortCls/WaveRT package is installed; the
checked-in ring contract alone is not a backend.

## Native video-codec boundary

The portable H.264 path accepts host-captured, checked I420 planes and emits
bounded Annex-B access units. `arcen-media` exposes a safe, pointer-free wrapper,
but `openh264-sys2` and the bundled Cisco C/C++ codec remain an unsafe native
memory boundary. Geometry, strides, output size, frame type, and recovery
parameter sets are validated around it; native initialization, encode, mixed
frame type, and oversized output failures close the media path rather than
switching codecs under an existing hello.

Only crates.io `openh264`/`openh264-sys2` 0.9.7 with the source feature are
admitted. No runtime loader, downloader, precompiled Cisco binary, codec
DLL/dylib/framework, or raw native pointer crosses into host policy. Exact
source and distribution controls are in
[`supply-chain.md`](supply-chain.md) and
[`../architecture/media-plan-resolution.md`](../architecture/media-plan-resolution.md).

## Deskside privacy boundary

Deskside is an operator-enforced host policy, not a Deck request and not a wire
capability. It is disabled by default. Enabling requires fresh positive
physical-console evidence and complete operator pins; absence of known VM
markers is never positive evidence. Missing, unknown, virtual, remote,
paravirtual, conflicting, stale, unpinned, or capture-overlapping resources
refuse before mutation. Input and display controls are atomic, and protected
state is not reported until both verify.

Both hosts require CPUID to report no hypervisor and require an operator-pinned
hash of positively parsed firmware/chassis facts. Linux independently binds one
local `Remote=no` seat0 console session and its UID/DISPLAY/Xauthority while the
dedicated streaming PAM session remains `Remote=yes`. These checks classify
expected bare-metal evidence; they do not resist a hostile hypervisor.

Windows performs privileged display recovery through its existing
SYSTEM/Administrators-only versioned journal/watchdog and process-owned hooks in
the authenticated session agent. Arcen `SendInput` carries a process-random
marker, and physical or unmarked foreign injected events are swallowed.
Keyboard and mouse liveness are separately proven by bounded swallowed
canaries, and evidence-bound topology is continuously checked while streaming
and reconnect-held. The marker is not a
security boundary against a hostile same-desktop process able to observe and
replay hook metadata. Linux keeps evdev
descriptors, continuously revalidates the relevant input inventory, and physical
console ownership in the root launcher and persists only bounded normalized
console restore facts in a root-only `/run/arcen` journal. Both reject
symlink/reparse paths. Neither path logs or persists input events, keys, raw
EDID, serials, configured `/dev/input/by-id` values, raw monitor device paths, or
customer hardware identifiers; only bounded hashes, counts, stages, and stable
non-sensitive refusal classes cross diagnostic boundaries.

Linux treats every key/button or relative-axis device as relevant, refuses
absolute-axis classes, binds pins to canonical node inode/device identity, and
continuously revalidates evdev plus complete DRM/EDID/xrandr/DPMS state. Any
device/output add, removal, replacement, wake, or parse uncertainty drains.
All xrandr/xset/loginctl children are bounded, killed, and reaped on timeout.
Journal I/O failure cannot skip unblank; live guard cancellation schedules
display-first cleanup and retains the journal on incomplete recovery.

Resumable transport loss holds the same resources and authoritative deadline;
it never rearms or replaces ownership. Terminal drain stops remote injection and
media first, restores displays before releasing input, attempts every restore
after a failure, and preserves failed recovery state. This ordering limits both
privacy gaps and local denial of service. SAS/secure desktop, kernel HID,
pen/tablet, physical hot-plug, sleep/resume, and driver-reset behavior are
explicit non-claims pending physical lab evidence.

## Direct-QUIC resume boundary

The draft SessionAutoReconnect implementation applies only to macOS Deck
directly connected to a Windows or Linux Pier over QUIC/TLS 1.3 on UDP 18444. An
opaque resume grant is a bearer secret: theft during its bounded lifetime can
attempt to take over the one detached session. TLS confidentiality is
necessary but not sufficient, so the grant additionally binds the validated
stable TLS SPKI host identity, active host session, native SID/WTS or
uid/logind principal, Deck holder nonce, disclaimer digest/version, and exact
client display/time-zone topology. It is not bound to a peer IP address.
Deck separately binds its memory-only controller to the configured endpoint,
TLS security mode/pin selection, topology, and connection generation. Every
attempt performs a fresh TLS handshake before sending the bearer grant; a TLS
pin or host-SPKI change is terminal rather than a compatibility fallback.

Each Pier process alone owns a fresh random 256-bit HMAC-SHA-256 key in
zeroizing memory. The key, grants, holder nonces, passwords, and credentials
must not be persisted, logged, formatted, or included in support bundles.
Typed debug output redacts secrets, received grant/nonce strings are cleared
after parsing, temporary token bytes and signatures are zeroized, and native
principal values remain outside safe telemetry. Session Log IDs and
`previous_sid` are diagnostic chaining only.

One mutex-protected current generation+nonce slot gives exactly one concurrent
winner and rotates before handoff or attached refresh. Replays, malformed
signatures, stale generations, and duplicate attempts fail closed. The
opted-in host refreshes the one slot every `W/2` and sends the successor in a
successful in-band `AuthResult`; claims remain bounded to `2W` and at most
14,400 seconds. No old and new slot are simultaneously acceptable.

Refresh and resume success have no wire acknowledgment. If either successful
`AuthResult` is lost, the old token is already consumed and the successor is
unknown to Deck. A known post-rotation serialization/send failure revokes and
drains immediately. Otherwise, presenting the cryptographically valid,
unexpired exact predecessor presented to a detached slot is visibly rejected as replay and atomically
triggers the same final drain. After cleanup, normal credentials can create a
session; the old token is never reopened and the successor is never recovered
or disclosed. Older, ahead, malformed, wrongly signed, expired, or incorrectly
bound candidates cannot trigger owner drain. Invalid resume never falls back to
PAM, `LogonUserW`, Credential Provider, ordinary password auth, or a new
disclaimer decision.

Deck stores no deadline while connected. At unexpected transport loss both
Deck and host start an exact monotonic, generation-tagged `W` deadline; failed
resume retries preserve that same deadline. Exact equality is expired, and
stale/early timer callbacks are no-ops. Clock acquisition, arithmetic,
randomness, HMAC, lock, or generation-exhaustion failures issue no usable grant
and fail closed.
Restart loses the process key and slot, making all pre-restart grants invalid;
there is no database, cross-process recovery, cross-host state, or network key
fetch.

Topology and native checks are repeated after token rotation and before media
attachment. TLS SPKI rekey, display/topology/time-zone change, SID/WTS or
uid/logind change, native-session death, or agent/launcher failure is terminal.
The initial disclaimer digest/version remains fixed through resume. Windows
retains its display watchdog/journal and system-wide time-zone lease; Linux
retains the dedicated-Xorg display resources and process-tree time-zone lease.
Final drain revokes resume before restoring/releasing them.

Adversarial unit tests cover token/signature tamper, every identity binding,
exact `2W` lifetime bounds, attached rotation, malformed/oversized tokens, one
concurrent rotation winner, exact-predecessor terminal replay, older-token
rejection, unauthenticated topology mismatch without attacker-triggered drain,
successor-send failures, stale timers, redaction, and one-time crash/shutdown
cleanup. Protocol tests cover old-peer JSON compatibility, strict in-band
refresh shapes, and stable failure strings; Deck tests restrict retries to
selected transient I/O and require a rotated successor before media. These
tests do not establish an
external audit, certification, production soak, real clock-manipulation proof,
or completed target-OS adversarial assessment.

See
[`../architecture/session-auto-reconnect.md`](../architecture/session-auto-reconnect.md)
and [ADR 0005](../adr/0005-direct-transport-resume-authority.md).

## Pre-session disclaimer

An enabled Pier sends exact, validated, bounded UTF-8 disclaimer text in the
protocol-v3 `AuthRequest`. Deck must display it before collecting credentials.
Only an explicit Accept may advance to credential entry and one `AuthResponse`
whose lowercase SHA-256 acknowledgment matches the exact displayed bytes.
Decline sends no `AuthResponse`; decline, close, timeout, absent acknowledgment,
invalid digest, and mismatch all fail before PAM, `LogonUserW`, Credential
Provider handoff, a privileged launcher, or session acquisition.

The acknowledgment is operational evidence, not authorization and not proof
that a human understood the text. It is not cryptographically bound into
`SessionGrantClaims` v1; a signed-schema v2 rollout is deferred. After successful
OS authentication, host logs may record only locale, digest, host acceptance
time, correlation/session identity, and success. Banner text, credentials,
username/domain/SID/PAM account, peer data, and raw errors are excluded.
Support bundles inherit the existing log privacy boundary and never add the
banner source file.

This behavior and operator-provided legal text require Shared/Architecture,
affected component-owner, and Release/Security/legal review. Arcen makes no
certification or legal-sufficiency claim for the configured wording.

The initial scaffold makes no claim of certification, regulatory compliance,
data residency, or completed cryptographic review.

## Support bundles

Support bundles are sensitive operational data, not anonymized reports. They
can contain redacted configuration, service state, lifecycle events, and raw
Arcen logs that may still identify usernames, domains, correlation IDs, or peer
addresses. Operators must protect bundles in storage and transit and review
them before sharing.

Collection is service-independent and uses per-platform source allowlists.
Credentials, TLS certificate/private-key files and configured paths,
Xauthority, customer payloads, proprietary payloads, arbitrary filesystem
trees, and hostnames in archive names/manifests are excluded. Collection must
not stat, open, hash, copy, or serialize either TLS material path or file. JSON
secret-bearing fields and native event host/security metadata are redacted,
while unavailable or denied sources remain visible as typed notices. Only
already-sanitized bounded TLS lifecycle metadata may enter through approved
logs/events.

Host adapters bound source counts and sizes, stream files with fixed buffers,
reject symlinks/reparse points, cap included logs at 200 MiB and total payload
at 256 MiB, and atomically publish from a same-directory partial file. Custom
`--out` paths are operator-selected trust boundaries and retain their existing
directory permissions.

CI runner, dependency, artifact, and release trust controls are detailed in
[Supply-chain security](supply-chain.md). Protected hardware and signing jobs
must remain disabled while their environment-review controls are unavailable.
