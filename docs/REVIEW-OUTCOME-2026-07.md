# Review outcome — backlog closed

The exhaustive architecture/security/performance review ran on 2026-07-25 to 2026-07-27 and produced 134 finding markdown files (133 ID-bearing findings plus the template) plus run scaffolding under `docs/todo/`. The review is now closed. This index is the durable replacement: fixed and deferred finding files were removed because their full text remains in git history; only live open findings remain as files.

Outcome counts for the 133 ID-bearing findings: **14 fixed**, **117 deferred**, **2 open**.

## Nothing remains open in this directory

The two findings that were still live are now tracked as GitHub issues, which is
where current work belongs:

| Was | Now | Severity |
| --- | --- | --- |
| PROTO-378 | the host serves its preferred codec, not the requested one | S3 |
| MEDIA-379 | `media-smoke` receives no video on h264 (diagnostic tool, not the product) | S3 |

BUILD-376 was resolved on 2026-07-27: the Deck is signed, notarised, stapled and
`spctl`-accepted on a quarantined copy.

`docs/todo/` no longer exists. Three documents in it were durable engineering
references rather than review scaffolding and were moved to their proper homes:

- `docs/architecture/ObservabilityStandard.md`
- `docs/architecture/LEXICON.md`
- `docs/DOC-STANDARD.md`

Every finding file was deleted. Their full text remains in git history at commit
`91b70c7` and earlier.

## Superseded: still open at the time of writing

| ID | Summary | Severity | Where |
| --- | --- | --- | --- |

| MEDIA-379 | `media-smoke` receives no video when the host serves h264 (diagnostic tool, not the product) | S3 | `docs/todo/findings/MEDIA-379.md` |
| PROTO-378 | the host serves its preferred codec, not the one the client asked for | S3 | `docs/todo/findings/PROTO-378.md` |

## Fixed findings

| ID | Summary | Severity | Outcome |
| --- | --- | --- | --- |
| BUILD-360 | Fail any commit that destroys documentation content or edits the provenance block | S2 | fixed in d9e8f76 with follow-up 619e58a (PR #62) |
| BUILD-372 | The shipped Windows default config triggers the Pier's own SEC-151 warning on first start | S3 | fixed in 64a4b8a and restored in c72cc82 (PR #78) |
| BUILD-375 | The Linux Pier has no software encode fallback compiled in; a host without NVIDIA cannot serve | S1 | fixed in 7d2b7f6/a88a11a and documented in 2912fdf (PR #79) |
| BUILD-376 | Arcen Deck.app double-clicks only on the machine that built it | S2 | resolved 2026-07-27: signed, notarised, stapled, spctl-accepted on a quarantined copy |
| BUILD-377 | A clean install on an NVIDIA Windows host cannot start a session | S1 | fixed in e1e71d8 (PR #88) |
| DEP-361 | Declare publish and license-file on the six binary crates so the licence gate means something | S3 | fixed in c4d983d (PR #65) |
| DOC-224 | Add ownership contracts to the raw IOKit wrapper types | S3 | fixed in 4eec226 (PR #64) |
| PERF-368 | Establish why the Linux Pier starts a stream that delivers no frames | S1 | closed as harness error in 2f7ef13 (PR #73) |
| SEC-001 | Make PAM the default Linux auth mode and delete the no-auth remote escape hatch | S2 | fixed in 3f163f7 (PR #63) |
| SEC-151 | Validate configured executable paths before SYSTEM spawns them | S2 | fixed in 64a4b8a and restored in c72cc82 (PR #78) |
| SEC-167 | Validate ProgramData config ownership before SYSTEM reads pier.json | S1 | fixed in 56ef6fa (PR #87) |
| SEC-209 | Stop releasing unretained IOHIDDeviceGetProperty results | S1 | fixed in 4eec226 (PR #64) |
| SEC-371 | The new Windows installer grants every local user read access to the host TLS private key | S1 | fixed in 486744f with validation e09e50e (PR #77/#78) |
| SEC-374 | The Windows single-binary capenc dispatch never emits READY, so every session fails | S1 | fixed by a173c2f and restored by 0bb7662 (PR #83) |

## Deferred findings

These had no merged fixing commit discoverable by finding ID in git history. They are deliberately skipped with the closed review backlog; re-file any that become product-relevant as new, current issues.

| ID | Summary | Severity | Outcome |
| --- | --- | --- | --- |
| API-001 | Stop advertising authentication methods that no host implements | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| API-259 | Pass negotiated FPS into NVENC initialization | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| API-305 | Replace primitive input identifiers and deltas with domain newtypes | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| API-312 | Reject zero keepalive cadences at construction | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-110 | Remove the crate-wide dead-code allowance from the Linux Pier | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-156 | Export Windows host modules from a library crate | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-159 | Split session.rs by ownership boundary | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-162 | Split Windows display control by transaction, topology, backend, and recovery seam | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-210 | Stop the HID run loop on session drop instead of waiting for a device removal callback | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-212 | Split ArcenApp into session, presentation, input, settings, and reconnect controllers | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-215 | Split the WebSocket session loop into transport, media, input, clipboard, microphone, and health actors | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-223 | Separate clipboard image decoding from the UI synchronization path | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-304 | Split absolute and relative pointer button and scroll events | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ARCH-315 | Make support-bundle output security a shared contract | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| BUILD-219 | Burn down arcen-deck-macos clippy warnings before enabling strict gates | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| BUILD-313 | Add a NASM-equipped CI lane for software-h264-source | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| BUILD-354 | Add strict Clippy gates for product crates, not only pure shared crates | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| BUILD-355 | Make the Rust toolchain contract explicit for default and optional feature builds | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| BUILD-356 | Either compile-gate or remove the dormant Rust packages that cannot be checked | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| BUILD-370 | Move Windows session-agent logs out of the install directory | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-001 | Give both Piers one licensing key-ring seam with one name and one shape | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-002 | Give the two Piers one name and one base for the display-selection config key | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-003 | Extract the duplicated resume registry, including its HMAC key, into shared/session | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-117 | Return the same expired-license resume error on Windows and Linux | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-157 | Align Windows session layout with Linux session seams | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-164 | Preserve license-expired resume rejection on Windows like Linux | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-166 | Use the same logging target names on Windows and Linux | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-181 | Make credential transcript length encoding checked like the identity transcript | S4 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-221 | Align Deck topology vocabulary with the active direct-only product scope | S4 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-263 | Unify capenc backend error propagation and fallback semantics | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-306 | Generate canonical telemetry field constants from the lifecycle schema | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| CONS-311 | Separate cumulative and windowed QoS counters | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DEP-261 | Record the NVENC header provenance and version generation inputs | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DEP-266 | Record the vendored NVIDIA NVENC binding in source provenance | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DEP-351 | Upgrade the vulnerable time dependency before shipping the workspace graph | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DEP-352 | Resolve the denied CC0-1.0 and MPL-2.0 license entries instead of carrying a failing policy | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DEP-353 | Deny duplicate dependency families after collapsing the GUI and platform crate graph | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DEP-357 | Add an owner and exit plan for the local opusic-sys fork on the audio data path | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DOC-109 | Report effective runtime diagnostics from configuration instead of hardcoded defaults | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DOC-116 | Add SAFETY comments to UHID byte-slice reinterprets | S4 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DOC-161 | Add SAFETY comments to SCM unsafe calls | S4 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DOC-314 | Document proven allocation counts beside hot-path APIs | S4 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| DOC-358 | Remove the stale claim that arcen-pier-windows cannot compile with current ServerHelloMsg | S4 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-103 | Reject oversized UHID descriptors and reports instead of truncating them | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-104 | Replace production audio encoder poison panics with typed shutdown | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-107 | Add deadlines to PulseAudio helper discovery operations | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-108 | Validate Linux monitor indices instead of saturating zero to output zero | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-111 | Treat malformed HID frames as protocol errors instead of silent ignores | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-112 | Return connection errors instead of panicking on remote-reachable session invariants | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-118 | Keep pre-exec closures to raw async-signal-safe syscalls | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-152 | Update input state only after SendInput succeeds | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-153 | Refresh SERVICE_STOP_PENDING while sessions drain | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-155 | Preserve capenc spawn error context | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-160 | Do not park microphone feeder threads forever on cleanup timeout | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-165 | Propagate timezone restore failure before completing resume drain | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-208 | Document and audit every VideoToolbox unsafe call before changing the decoder again | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-213 | Replace poison-panic mutex handling in production media paths with fail-closed session errors | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-220 | Remove production unreachable panic from audio stream handling | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-253 | Replace NVENC function-table unwraps with checked startup invariants | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-254 | Close libcuda on every CUDA startup error | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-255 | Propagate WGC acquisition failures in the Media Foundation loop | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-264 | Release DXGI frames on null-resource and callback failure paths | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-310 | Make observability sink timeouts cancel or quarantine blocked workers | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| ERR-366 | Report health as available while a session is actually streaming | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-105 | Move audio packet buffers out of the 20 ms encode hot path | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-106 | Remove the shared mutex from the audio encoder fast path | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-113 | Release the audio capture mutex before awaiting client control sends | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-154 | Stop copying every encoded frame twice | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-163 | Cap display topology allocations before trusting Win32 reported counts | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-200 | Keep decoded frames on the GPU path instead of copying every pixel through RGBA Vecs | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-201 | Preserve WebSocket binary buffers through the media inbox instead of cloning payloads | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-202 | Stop cloning telemetry queues and strings while holding the media-state mutex each repaint | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-203 | Replace the CoreAudio playback Mutex and VecDeque with a real-time safe ring buffer | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-204 | Bound and coalesce outbound UI input instead of sending it through an unbounded command queue | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-205 | Move network probing out of the transport select loop | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-211 | Do not block the media worker on VideoToolbox asynchronous completion for every packet | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-214 | Remove heap JSON serialization from every input event | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-216 | Bound HID report delivery and avoid per-report heap copies in the callback | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-218 | Avoid per-repaint heap work when aligning native key metadata | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-222 | Pool AVCC sample buffers instead of allocating a new access-unit Vec per decode | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-225 | Do not format per-frame summaries on the media worker hot path | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-251 | Stop flushing stdout after every access unit | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-257 | Reuse Media Foundation Annex-B output storage per frame | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-258 | Avoid the extra retained-texture copy on every Windows NVENC frame | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-260 | Remove the redundant XGetImage fallback copy | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-301 | Add a vectorized BGRA to YUV420 conversion path | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-302 | Do not hold the observability sequence mutex while routing records | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-303 | Remove unconditional per-event formatting and cloning from record routing | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| PERF-365 | Stop waiting a fixed 15 seconds for desktop stability after first login | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-002 | Delete the unverified challenge/response auth path or implement verification | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-100 | Reject untrusted Pier configuration files before applying privileged overrides | S1 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-101 | Execute only absolute reviewed helper and diagnostic binaries | S1 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-102 | Reopen the microphone FIFO with nofollow descriptor validation | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-114 | Execute support-bundle diagnostics by absolute path | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-115 | Stop removing global X11 lock and socket paths during session cleanup | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-150 | Run the session agent with least privilege | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-158 | Scope SeTimeZonePrivilege to a reversible token guard | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-180 | Remove the credential-provider harness from release packages | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-182 | Align TOKEN_USER buffers before casting them to Win32 token structs | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-206 | Remove plaintext passwords from command-line arguments | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-207 | Build insecure TLS out of production instead of double-gating it at runtime | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-217 | Keep USB serial numbers out of default Info diagnostics | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-250 | Bound capenc control-line length before allocation | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-252 | Validate NVENC bitstream pointers before slicing driver memory | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-256 | Guard Media Foundation buffer locks with RAII | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-262 | Cap input-helper stdio command length before JSON escaping | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-265 | Bound Windows Pier capenc stderr reads before READY and log forwarding | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-300 | Redact diagnostic values before they enter support bundles | S1 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-350 | Stop passing the issuer private-key path through helper argv | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-363 | Rotate the pier-windows-software.example.internal credential exposed in the 2026-07-26 session transcript | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-364 | Tell the user when live display resize is unavailable instead of silently degrading | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-367 | Reap the dedicated Xorg session when the client disconnects | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| SEC-373 | Second credential leak by me: a line-wise redaction applied to a multi-line value | S1 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| TEST-307 | Add fuzz targets for remote media and control parsers | S2 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| TEST-308 | Prove Opus encoder steady-state allocations under the audio-opus feature | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| TEST-309 | Gate Keel Auto hash selection with measured corpus results | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |
| TEST-362 | Isolate the Linux licensing tests from the machine's real licence state | S3 | deferred — no merged fixing commit with this ID was found; old review backlog closed/skipped unless re-filed |

## Inventory note

The original finding `status:` fields were inventoried before pruning. Most older files had no status field; the live late findings carried `status: open`. Git history, not the stale file status, determined this outcome table.

## Durable documents kept

- `LEXICON.md` — canonical vocabulary still useful for future rename work.
- `DOC-STANDARD.md` — documentation standard still in force.
- `SECURITY.md` — durable security review reference; individual backlog items above are closed or deferred.
- `ObservabilityStandard.md` — implemented observability standard and historical decomposition.

Review-run scaffolding, completed feature blueprints, progress/state logs, PR scratch notes, and the findings template were deleted.
